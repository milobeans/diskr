use anyhow::Result;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::scanner::{ScanId, ScanMsg, Scanner};

const SORT_DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Files,
    Disks,
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
    pub size: Option<u64>,
    pub modified: Option<std::time::SystemTime>,
    pub scanning: bool,
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
    pub show_hidden: bool,
    pub sort: SortMode,
    pub focus: Focus,
    pub disks: Vec<DiskInfo>,
    pub status: String,
    pub confirming_delete: bool,

    size_cache: HashMap<PathBuf, u64>,
    last_sort: Instant,
    sort_dirty: bool,
    active_scan_id: ScanId,

    scan_rx: Receiver<ScanMsg>,
    scanner: Scanner,
}

impl App {
    pub fn new(cwd: PathBuf) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App {
            cwd,
            entries: Vec::new(),
            selected: 0,
            show_hidden: false,
            sort: SortMode::SizeDesc,
            focus: Focus::Files,
            disks: Vec::new(),
            status: String::from("q to quit · r to rescan · d to trash"),
            confirming_delete: false,
            size_cache: HashMap::new(),
            last_sort: Instant::now(),
            sort_dirty: false,
            active_scan_id: 0,
            scanner: Scanner::new(tx.clone()),
            scan_rx: rx,
        };
        app.refresh_disks();
        app.reload()?;
        Ok(app)
    }

    pub fn reload(&mut self) -> Result<()> {
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
                        Some(m.len())
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
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.auto_scan();
        Ok(())
    }

    pub fn apply_sort(&mut self) {
        match self.sort {
            SortMode::Name => self.entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            }),
            SortMode::SizeDesc => self.entries.sort_by_key(|e| Reverse(e.size.unwrap_or(0))),
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
        self.sort = match self.sort {
            SortMode::Name => SortMode::SizeDesc,
            SortMode::SizeDesc => SortMode::Modified,
            SortMode::Modified => SortMode::Name,
        };
        self.apply_sort_preserving_selection();
        self.status = format!("sort: {}", self.sort.label());
    }

    pub fn move_cursor(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as i32;
        let s = self.selected as i32 + delta;
        self.selected = s.rem_euclid(n) as usize;
    }

    pub fn enter(&mut self) -> Result<()> {
        if let Some(entry) = self.entries.get(self.selected).cloned() {
            if entry.is_dir {
                self.cwd = entry.path;
                self.selected = 0;
                self.reload()?;
            }
        }
        Ok(())
    }

    pub fn go_up(&mut self) -> Result<()> {
        if let Some(parent) = self.cwd.parent().map(|p| p.to_path_buf()) {
            self.cwd = parent;
            self.selected = 0;
            self.reload()?;
        }
        Ok(())
    }

    pub fn toggle_hidden(&mut self) -> Result<()> {
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
        self.status = format!("scanning {} directories…", dirs.len());
        self.scanner.scan_all(scan_id, dirs);
    }

    /// Invoked by the `r` key. Invalidates cache for everything in view, rescans all.
    pub fn force_rescan(&mut self) {
        let scan_id = self.next_scan_id();
        for e in self.entries.iter().filter(|e| e.is_dir) {
            self.size_cache.remove(&e.path);
        }
        for e in self.entries.iter_mut().filter(|e| e.is_dir) {
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
            self.status = String::from("no directories to rescan");
            return;
        }
        self.status = format!("rescan: {} directories…", dirs.len());
        self.scanner.scan_all(scan_id, dirs);
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
        changed
    }

    pub fn has_pending_scan_work(&self) -> bool {
        self.sort_dirty || self.entries.iter().any(|entry| entry.scanning)
    }

    pub fn request_delete(&mut self) {
        if self.entries.get(self.selected).is_some() {
            self.confirming_delete = true;
        }
    }

    pub fn cancel_delete(&mut self) {
        self.confirming_delete = false;
    }

    pub fn confirm_delete(&mut self) -> Result<()> {
        self.confirming_delete = false;
        if let Some(entry) = self.entries.get(self.selected).cloned() {
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
        Ok(())
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

    pub fn refresh_disks(&mut self) {
        self.disks = disk_info();
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
            let block_size = u64::from(stat.f_bsize);
            let total = stat.f_blocks.saturating_mul(block_size);
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

#[allow(dead_code)]
pub fn is_under(p: &Path, root: &Path) -> bool {
    p.starts_with(root)
}
