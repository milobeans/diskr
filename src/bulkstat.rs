//! macOS-native directory-size walker using `getattrlistbulk(2)`.
//!
//! Instead of `readdir` + one `stat` per file (the usual pattern), this issues a
//! single syscall that returns packed attributes for dozens of entries at once.
//! Batching reduces per-file kernel crossings. Actual throughput depends on
//! the filesystem, tree shape and cache state; benchmark representative trees.
//!
//! Layout of a fully valid entry. Invalid attributes have no physical slot;
//! the parser advances only over returned bits (PACK_INVAL_ATTRS is not used).
//!   [ 0.. 4] length          u32   — total length of this entry including padding
//!   [ 4..24] returned_attrs  5*u32 — bitmap of which attrs the kernel filled in
//!   [24..28] per-entry error u32   — present when returned_attrs says so
//!   [28..36] name ref        i32+u32 — offset+length pointing to name bytes below
//!   [36..40] devid           i32   — device number
//!   [40..44] objtype         u32   — VREG=1, VDIR=2, VLNK=5, …
//!   [44..52] fileid          u64   — inode-equivalent id
//!   [52..56] linkcount       u32   — present for regular files
//!   [56..64] totalsize       u64   — present for regular files
//!   [64..72] allocsize       u64   — present for regular files
//!   [..    ] name bytes + padding

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};

use crate::pool::TaskGroup;

// sys/attr.h constants (stable macOS ABI)
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x00000001;
const ATTR_CMN_DEVID: u32 = 0x00000002;
const ATTR_CMN_OBJTYPE: u32 = 0x00000008;
const ATTR_CMN_FILEID: u32 = 0x02000000;
const ATTR_CMN_ERROR: u32 = 0x20000000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x80000000;
const ATTR_FILE_LINKCOUNT: u32 = 0x00000001;
const ATTR_FILE_TOTALSIZE: u32 = 0x00000002;
const ATTR_FILE_ALLOCSIZE: u32 = 0x00000004;
const FSOPT_RETURN_REALDEV: u64 = 0x00000200;
const ATTRIBUTE_SET_LEN: usize = 20;
const ATTR_REFERENCE_LEN: usize = 8;
const DATA_VOLUME_ROOT: &str = "/System/Volumes/Data";

#[cfg(test)]
static SCANNER_SYSCALLS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn count_scanner_syscall() {
    #[cfg(test)]
    SCANNER_SYSCALLS.fetch_add(1, Ordering::Relaxed);
}

// vnode types (sys/vnode.h)
const VREG: u32 = 1;
const VDIR: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeInfo {
    pub logical: u64,
    pub allocated: u64,
}

impl SizeInfo {
    pub fn new(logical: u64, allocated: u64) -> Self {
        Self { logical, allocated }
    }

    fn add_file(&mut self, file: SizeInfo) {
        self.logical = self.logical.saturating_add(file.logical);
        self.allocated = self.allocated.saturating_add(file.allocated);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LargeFile {
    pub path: PathBuf,
    pub size: SizeInfo,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirScan {
    pub size: SizeInfo,
    pub largest_files: Vec<LargeFile>,
    /// Directories or entries that could not be traversed or read. When
    /// non-zero the reported size is a lower bound.
    pub inaccessible: u32,
    /// Subdirectories skipped because they sit on a different volume than the
    /// scan root (external/network/FUSE mounts and APFS helper volumes such as
    /// Preboot/VM/Update), to avoid counting another filesystem's bytes.
    pub skipped_mounts: u32,
    pub hard_links: Vec<(u64, u64, SizeInfo)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryCoverage {
    pub depth_limit: usize,
    pub depth_limited: u32,
    pub skipped_mounts: u32,
    pub excluded_policy: u32,
}

impl DiscoveryCoverage {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({ "policy": "same selected filesystem; no symlinks; tool-specific hidden/cache exclusions", "depth_limit": self.depth_limit, "depth_limited": self.depth_limited, "skipped_mounts": self.skipped_mounts, "excluded_policy": self.excluded_policy })
    }
    pub fn summary(&self) -> String {
        format!(
            "scope: depth {} · {} depth-limited · {} mounts skipped · {} policy exclusions",
            self.depth_limit, self.depth_limited, self.skipped_mounts, self.excluded_policy
        )
    }
}

#[derive(Clone, Debug)]
pub struct ScanCancellation {
    generation: Arc<AtomicU64>,
    expected: u64,
    clients: Option<Arc<Mutex<Vec<ScanCancellation>>>>,
}

impl ScanCancellation {
    pub fn new(generation: Arc<AtomicU64>, expected: u64) -> Self {
        Self {
            generation,
            expected,
            clients: None,
        }
    }

    pub fn never() -> Self {
        Self::new(Arc::new(AtomicU64::new(0)), 0)
    }

    pub fn is_cancelled(&self) -> bool {
        if let Some(clients) = &self.clients {
            return clients
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .all(Self::is_cancelled);
        }
        self.generation.load(Ordering::Relaxed) != self.expected
    }
}

#[repr(C)]
struct Attrlist {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

/// Recursive size and optional largest-file report for `root`.
///
/// Counts regular-file logical and allocated sizes only. Symlinks, the
/// directory inodes' own allocated blocks, and other special files contribute
/// nothing, so the total is content-only and runs slightly under `du`/Finder
/// on directory-heavy trees. Directories that cannot be opened
/// and individual entries whose attributes cannot be read are counted in
/// `inaccessible`; when it is non-zero the reported size is a lower bound.
/// Subdirectories on a different volume than the scan root
/// are skipped and counted in `skipped_mounts`.
pub fn scan_dir(root: &Path, top_file_limit: usize) -> DirScan {
    scan_dir_with_cancellation(root, top_file_limit, &ScanCancellation::never()).unwrap_or_default()
}

pub fn scan_dir_with_cancellation(
    root: &Path,
    top_file_limit: usize,
    cancellation: &ScanCancellation,
) -> Option<DirScan> {
    shared_scan_sync(root, top_file_limit, cancellation, None)
}

pub(crate) fn scan_dir_in_scope(
    root: &Path,
    top_file_limit: usize,
    cancellation: &ScanCancellation,
    scope: Option<&ScanScope>,
) -> Option<DirScan> {
    if scope.is_none_or(|scope| scope.independent) {
        return shared_scan_sync(
            root,
            top_file_limit,
            cancellation,
            scope.map(|scope| scope.context.policy),
        );
    }
    if cancellation.is_cancelled() {
        return None;
    }
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Some(DirScan::default());
        }
        Err(_) => {
            return Some(DirScan {
                inaccessible: 1,
                ..DirScan::default()
            });
        }
    };
    if !meta.file_type().is_dir() {
        return Some(DirScan::default());
    }

    let policy_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut walk = WalkState::new(
        ScanPolicy::new(&policy_root, meta.dev()),
        top_file_limit,
        cancellation.clone(),
    );
    if let Some(scope) = scope {
        walk.context = Arc::clone(&scope.context);
    }
    let state = Arc::new(walk);
    let group = TaskGroup::new();
    spawn_walk(root.to_path_buf(), Arc::clone(&state), &group);
    group.wait();

    if cancellation.is_cancelled() {
        return None;
    }
    Some(state.finish())
}

/// Asynchronous scan on the shared pool. `on_start` runs when the walk is
/// dequeued; `on_done` runs on the worker that finishes last, with `None`
/// when the scan was cancelled. Never blocks the calling thread.
pub fn scan_dir_async(
    root: PathBuf,
    top_file_limit: usize,
    cancellation: ScanCancellation,
    interactive: bool,
    on_start: impl FnOnce() + Send + 'static,
    on_done: impl FnOnce(Option<DirScan>) + Send + 'static,
) {
    let group = TaskGroup::new();
    let task = group.spawn_promotable(move || {
        shared_scan_async(
            root,
            top_file_limit,
            cancellation,
            None,
            interactive,
            on_start,
            on_done,
        )
    });
    if interactive {
        task.promote();
    }
    group.arm();
}

fn scan_dir_async_unshared(
    root: PathBuf,
    top_file_limit: usize,
    cancellation: ScanCancellation,
    policy: Option<ScanPolicy>,
    on_start: impl FnOnce() + Send + 'static,
    on_done: impl FnOnce(Option<DirScan>) + Send + 'static,
) -> crate::pool::Promotion {
    let result_state: Arc<Mutex<Option<Arc<WalkState>>>> = Arc::new(Mutex::new(None));
    let finish_state = Arc::clone(&result_state);
    let finish_cancellation = cancellation.clone();
    let group = TaskGroup::with_finish(move || {
        if finish_cancellation.is_cancelled() {
            on_done(None);
            return;
        }
        let state = finish_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        // No state means the root was missing or not a directory; report an
        // empty scan, matching the sync path.
        on_done(Some(state.map(|state| state.finish()).unwrap_or_default()));
    });
    let walk_group = Arc::clone(&group);
    let promotion = group.spawn_promotable(move || {
        if cancellation.is_cancelled() {
            return;
        }
        on_start();
        let meta = match std::fs::symlink_metadata(&root) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {
                let state = Arc::new(WalkState::new(
                    ScanPolicy::new(&root, 0),
                    top_file_limit,
                    cancellation.clone(),
                ));
                state.inaccessible.store(1, Ordering::Relaxed);
                *result_state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state);
                return;
            }
        };
        if !meta.file_type().is_dir() {
            return;
        }
        let policy_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let state = Arc::new(WalkState::new(
            policy.unwrap_or_else(|| ScanPolicy::new(&policy_root, meta.dev())),
            top_file_limit,
            cancellation.clone(),
        ));
        *result_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&state));
        walk_task(root, state, walk_group);
    });
    group.arm();
    promotion
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SharedScanKey {
    root: PathBuf,
    device: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    policy: ScanPolicy,
    limit: usize,
}

type StartCallback = Box<dyn FnOnce() + Send>;
type DoneCallback = Box<dyn FnOnce(Option<DirScan>) + Send>;
type SharedRegistry = Mutex<HashMap<SharedScanKey, std::sync::Weak<SharedWalk>>>;

#[derive(Default)]
struct SharedCallbacks {
    started: bool,
    finished: bool,
    subscribers: Vec<Arc<ScanSubscriber>>,
}

struct ScanSubscriber {
    cancellation: ScanCancellation,
    start: Mutex<Option<StartCallback>>,
    done: Mutex<Option<DoneCallback>>,
}

impl ScanSubscriber {
    fn start(&self) {
        let mut start = recover_lock(&self.start);
        if let Some(callback) = start.take() {
            if !self.cancellation.is_cancelled() {
                callback();
            }
        }
    }

    fn finish(&self, result: &Option<DirScan>) {
        self.start();
        let callback = recover_lock(&self.done).take();
        if let Some(callback) = callback {
            callback(if self.cancellation.is_cancelled() {
                None
            } else {
                result.clone()
            });
        }
    }
}

struct SharedWalk {
    clients: Arc<Mutex<Vec<ScanCancellation>>>,
    callbacks: Mutex<SharedCallbacks>,
    promotion: Mutex<Option<crate::pool::Promotion>>,
    interactive: std::sync::atomic::AtomicBool,
}

fn shared_registry() -> &'static SharedRegistry {
    static REGISTRY: std::sync::OnceLock<SharedRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn recover_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

impl SharedWalk {
    fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(Vec::new())),
            callbacks: Mutex::new(SharedCallbacks::default()),
            promotion: Mutex::new(None),
            interactive: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn promote(&self) {
        self.interactive.store(true, Ordering::Relaxed);
        if let Some(promotion) = recover_lock(&self.promotion).clone() {
            promotion.promote();
        }
    }

    fn start(&self) {
        let subscribers = {
            let mut callbacks = recover_lock(&self.callbacks);
            callbacks.started = true;
            callbacks.subscribers.clone()
        };
        for subscriber in subscribers {
            subscriber.start();
        }
    }

    fn finish(&self) -> Vec<Arc<ScanSubscriber>> {
        let mut callbacks = recover_lock(&self.callbacks);
        callbacks.finished = true;
        std::mem::take(&mut callbacks.subscribers)
    }
}

pub(crate) fn invalidate_shared_scans(path: &Path) {
    recover_lock(shared_registry())
        .retain(|key, _| !key.root.starts_with(path) && !path.starts_with(&key.root));
}

pub(crate) fn prioritize_scan(path: &Path) {
    let jobs: Vec<_> = recover_lock(shared_registry())
        .iter()
        .filter(|(key, _)| key.root == path)
        .filter_map(|(_, job)| job.upgrade())
        .collect();
    for job in jobs {
        job.promote();
    }
}

fn shared_scan_sync(
    root: &Path,
    limit: usize,
    cancellation: &ScanCancellation,
    policy: Option<ScanPolicy>,
) -> Option<DirScan> {
    let (tx, rx) = std::sync::mpsc::channel();
    shared_scan_async(
        root.to_path_buf(),
        limit,
        cancellation.clone(),
        policy,
        false,
        || {},
        move |result| {
            let _ = tx.send(result);
        },
    );
    loop {
        if cancellation.is_cancelled() {
            return None;
        }
        match rx.try_recv() {
            Ok(result) => {
                return if cancellation.is_cancelled() {
                    None
                } else {
                    result
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if !crate::pool::help_once() {
            match rx.recv_timeout(std::time::Duration::from_millis(2)) {
                Ok(result) => {
                    return if cancellation.is_cancelled() {
                        None
                    } else {
                        result
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

fn shared_scan_async(
    root: PathBuf,
    limit: usize,
    cancellation: ScanCancellation,
    policy: Option<ScanPolicy>,
    interactive: bool,
    on_start: impl FnOnce() + Send + 'static,
    on_done: impl FnOnce(Option<DirScan>) + Send + 'static,
) {
    if cancellation.is_cancelled() {
        on_done(None);
        return;
    }
    let meta = match std::fs::symlink_metadata(&root) {
        Ok(meta) if meta.is_dir() => meta,
        _ => {
            let promotion =
                scan_dir_async_unshared(root, limit, cancellation, policy, on_start, on_done);
            if interactive {
                promotion.promote();
            }
            return;
        }
    };
    let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
    let policy = policy.unwrap_or_else(|| ScanPolicy::new(&canonical, meta.dev()));
    let key = SharedScanKey {
        root: root.clone(),
        device: meta.dev(),
        inode: meta.ino(),
        modified: (meta.mtime(), meta.mtime_nsec()),
        changed: (meta.ctime(), meta.ctime_nsec()),
        policy,
        limit,
    };
    let mut registry = recover_lock(shared_registry());
    registry.retain(|_, job| job.strong_count() > 0);
    let existing = registry
        .get(&key)
        .and_then(std::sync::Weak::upgrade)
        .filter(|job| {
            if recover_lock(&job.callbacks).finished {
                return false;
            }
            let mut clients = recover_lock(&job.clients);
            if clients.iter().all(ScanCancellation::is_cancelled) {
                return false;
            }
            clients.push(cancellation.clone());
            true
        });
    let leader = existing.is_none();
    if leader && registry.len() >= 512 {
        drop(registry);
        let promotion =
            scan_dir_async_unshared(root, limit, cancellation, Some(policy), on_start, on_done);
        if interactive {
            promotion.promote();
        }
        return;
    }
    let job = existing.unwrap_or_else(|| Arc::new(SharedWalk::new()));
    registry.insert(key.clone(), Arc::downgrade(&job));
    if leader {
        recover_lock(&job.clients).push(cancellation.clone());
    }
    let subscriber = Arc::new(ScanSubscriber {
        cancellation: cancellation.clone(),
        start: Mutex::new(Some(Box::new(on_start))),
        done: Mutex::new(Some(Box::new(on_done))),
    });
    let started = {
        let mut callbacks = recover_lock(&job.callbacks);
        callbacks.subscribers.push(Arc::clone(&subscriber));
        callbacks.started
    };
    drop(registry);
    if started {
        subscriber.start();
    }
    if interactive {
        job.promote();
    }
    if !leader {
        return;
    }
    let mut shared_cancellation = ScanCancellation::never();
    shared_cancellation.clients = Some(Arc::clone(&job.clients));
    let start_job = Arc::clone(&job);
    let finish_job = Arc::clone(&job);
    let promotion = scan_dir_async_unshared(
        root,
        limit,
        shared_cancellation,
        Some(policy),
        move || start_job.start(),
        move |result| {
            let mut registry = recover_lock(shared_registry());
            let subscribers = finish_job.finish();
            if registry
                .get(&key)
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|current| Arc::ptr_eq(&current, &finish_job))
            {
                registry.remove(&key);
            }
            drop(registry);
            for subscriber in subscribers {
                subscriber.finish(&result);
            }
        },
    );
    *recover_lock(&job.promotion) = Some(promotion.clone());
    if job.interactive.load(Ordering::Relaxed) {
        promotion.promote();
    }
}

/// Shared accumulator for one walk. Sizes and counters are relaxed atomics
/// so completed directories merge without a global lock; the
/// largest-files heap keeps a mutex but is only touched when a top-files
/// report was requested.
struct WalkState {
    context: Arc<ScanContext>,
    top_file_limit: usize,
    cancellation: ScanCancellation,
    logical: AtomicU64,
    allocated: AtomicU64,
    inaccessible: AtomicU32,
    skipped_mounts: AtomicU32,
    largest_files: Mutex<BinaryHeap<Reverse<FileCandidate>>>,
}

impl WalkState {
    fn new(policy: ScanPolicy, top_file_limit: usize, cancellation: ScanCancellation) -> Self {
        Self {
            context: Arc::new(ScanContext {
                policy,
                seen_hard_links: Mutex::new(HashMap::new()),
            }),
            top_file_limit,
            cancellation,
            logical: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            inaccessible: AtomicU32::new(0),
            skipped_mounts: AtomicU32::new(0),
            largest_files: Mutex::new(BinaryHeap::new()),
        }
    }

    fn merge(&self, partial: DirectoryScan) {
        self.logical
            .fetch_add(partial.size.logical, Ordering::Relaxed);
        self.allocated
            .fetch_add(partial.size.allocated, Ordering::Relaxed);
        if partial.inaccessible > 0 {
            self.inaccessible
                .fetch_add(partial.inaccessible, Ordering::Relaxed);
        }
        if partial.skipped_mounts > 0 {
            self.skipped_mounts
                .fetch_add(partial.skipped_mounts, Ordering::Relaxed);
        }
        if self.top_file_limit == 0 || partial.largest_files.is_empty() {
            return;
        }
        let mut heap = self
            .largest_files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for Reverse(candidate) in partial.largest_files {
            push_file_candidate(&mut heap, self.top_file_limit, candidate);
        }
    }

    fn finish(&self) -> DirScan {
        let heap = std::mem::take(
            &mut *self
                .largest_files
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        DirScan {
            size: SizeInfo::new(
                self.logical.load(Ordering::Relaxed),
                self.allocated.load(Ordering::Relaxed),
            ),
            largest_files: sorted_largest_files(heap),
            inaccessible: self.inaccessible.load(Ordering::Relaxed),
            skipped_mounts: self.skipped_mounts.load(Ordering::Relaxed),
            hard_links: self
                .context
                .seen_hard_links
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(&(dev, ino), &size)| (dev, ino, size))
                .collect(),
        }
    }
}

#[derive(Default)]
struct DirectoryScan {
    size: SizeInfo,
    largest_files: BinaryHeap<Reverse<FileCandidate>>,
    subdirs: Vec<PathBuf>,
    inaccessible: u32,
    skipped_mounts: u32,
}

impl DirectoryScan {
    fn merge(&mut self, other: Self, limit: usize) {
        self.size.add_file(other.size);
        self.inaccessible = self.inaccessible.saturating_add(other.inaccessible);
        self.skipped_mounts = self.skipped_mounts.saturating_add(other.skipped_mounts);
        self.subdirs.extend(other.subdirs);
        for Reverse(candidate) in other.largest_files {
            push_file_candidate(&mut self.largest_files, limit, candidate);
        }
    }
}

struct ScanContext {
    policy: ScanPolicy,
    seen_hard_links: Mutex<HashMap<FileIdentity, SizeInfo>>,
}

#[derive(Clone)]
pub(crate) struct ScanScope {
    context: Arc<ScanContext>,
    independent: bool,
}

impl ScanScope {
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        let meta = std::fs::symlink_metadata(root)?;
        Ok(Self {
            independent: false,
            context: Arc::new(ScanContext {
                policy: ScanPolicy::new(root, meta.dev()),
                seen_hard_links: Mutex::new(HashMap::new()),
            }),
        })
    }
    pub(crate) fn fresh(&self) -> Self {
        Self {
            independent: true,
            context: Arc::new(ScanContext {
                policy: self.context.policy,
                seen_hard_links: Mutex::new(HashMap::new()),
            }),
        }
    }
    pub(crate) fn accepts(&self, path: &Path, meta: &std::fs::Metadata) -> bool {
        !should_skip_subdir(path, Some(meta.dev()), &self.context.policy)
    }
    pub(crate) fn file_size(&self, meta: &std::fs::Metadata) -> SizeInfo {
        let size = SizeInfo::new(meta.len(), meta.blocks().saturating_mul(512));
        if should_count_file(
            Some(meta.dev()),
            Some(meta.ino()),
            meta.nlink() as u32,
            &self.context,
            size,
        ) {
            size
        } else {
            SizeInfo::default()
        }
    }
}

type FileIdentity = (u64, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ScanPolicy {
    root_dev: u64,
    /// Device of the APFS Data volume (`/System/Volumes/Data`) when it resolves.
    /// On modern macOS the root (System) volume and the Data volume are
    /// firmlinked into a single logical `/`, so both devices belong to the
    /// scanned filesystem and must be counted; any other device is a separate
    /// mount.
    data_dev: Option<u64>,
    skip_data_volume: bool,
}

impl ScanPolicy {
    fn new(root: &Path, root_dev: u64) -> Self {
        let data_dev = std::fs::symlink_metadata(DATA_VOLUME_ROOT)
            .ok()
            .map(|meta| meta.dev());
        Self::with_devices(root, root_dev, data_dev)
    }

    fn with_devices(root: &Path, root_dev: u64, data_dev: Option<u64>) -> Self {
        let data_root = Path::new(DATA_VOLUME_ROOT);
        Self {
            root_dev,
            data_dev,
            skip_data_volume: data_root.starts_with(root) && root != data_root,
        }
    }
}

fn spawn_walk(dir: PathBuf, state: Arc<WalkState>, group: &Arc<TaskGroup>) {
    let walk_group = Arc::clone(group);
    group.spawn(move || walk_task(dir, state, walk_group));
}

/// Scans `dir`, then keeps descending into one subdirectory per iteration
/// while handing the remaining siblings to the pool in a single batch.
/// The inline continuation keeps deep chains on one worker, cuts queue
/// traffic to one batched push per branching directory, and lets the chain
/// accumulate locally so shared `WalkState` counters are touched once per
/// chain instead of once per directory.
///
/// While descending, the child is opened with `openat(2)` relative to the
/// held parent fd, so the kernel resolves one path component instead of
/// re-walking the full path per directory — on deep trees plain `open(2)`
/// is a large share of scan time. Only chain descent holds an fd; spawned
/// branch tasks reopen by full path, keeping open fds bounded by worker
/// count.
fn walk_task(mut dir: PathBuf, state: Arc<WalkState>, group: Arc<TaskGroup>) {
    let mut chain = DirectoryScan::default();
    let mut fd = match open_dir(&dir) {
        Ok(fd) => Some(fd),
        Err(errno) => {
            chain.inaccessible = inaccessible_from_errno(errno);
            None
        }
    };
    while let Some(current) = fd.take() {
        if state.cancellation.is_cancelled() {
            return;
        }
        let mut opened = std::mem::MaybeUninit::<libc::stat>::uninit();
        count_scanner_syscall();
        if unsafe { libc::fstat(current.0, opened.as_mut_ptr()) } != 0 {
            chain.inaccessible = chain.inaccessible.saturating_add(1);
            break;
        }
        let opened = unsafe { opened.assume_init() };
        if should_skip_subdir(
            &dir,
            Some(opened.st_dev as u32 as u64),
            &state.context.policy,
        ) {
            chain.skipped_mounts = chain.skipped_mounts.saturating_add(1);
            break;
        }
        let mut partial = scan_open_dir(
            &current,
            &dir,
            state.top_file_limit,
            &state.context,
            &state.cancellation,
        );
        let mut subdirs = std::mem::take(&mut partial.subdirs);
        if state.cancellation.is_cancelled() {
            return;
        }
        chain.size.add_file(partial.size);
        chain.inaccessible = chain.inaccessible.saturating_add(partial.inaccessible);
        chain.skipped_mounts = chain.skipped_mounts.saturating_add(partial.skipped_mounts);
        for Reverse(candidate) in partial.largest_files {
            push_file_candidate(&mut chain.largest_files, state.top_file_limit, candidate);
        }
        let next = subdirs.pop();
        if !subdirs.is_empty() {
            let tasks: Vec<_> = subdirs
                .into_iter()
                .map(|subdir| {
                    let state = Arc::clone(&state);
                    let group = Arc::clone(&group);
                    move || walk_task(subdir, state, group)
                })
                .collect();
            group.spawn_all(tasks);
        }
        if let Some(subdir) = next {
            match subdir.file_name().map(|name| open_subdir(&current, name)) {
                Some(Ok(child)) => {
                    fd = Some(child);
                    dir = subdir;
                }
                Some(Err(errno)) => {
                    chain.inaccessible = chain
                        .inaccessible
                        .saturating_add(inaccessible_from_errno(errno));
                }
                None => {}
            }
        }
    }
    state.merge(chain);
}

/// Owned directory fd, closed on drop.
struct DirFd(libc::c_int);

impl Drop for DirFd {
    fn drop(&mut self) {
        count_scanner_syscall();
        unsafe { libc::close(self.0) };
    }
}

/// Every open/traversal error except ENOENT means data may have been skipped and
/// must make the result a lower bound. ENOENT is a benign deletion race.
fn inaccessible_from_errno(errno: Option<i32>) -> u32 {
    u32::from(!matches!(errno, Some(libc::ENOENT)))
}

fn open_dir(dir: &Path) -> Result<DirFd, Option<i32>> {
    let c_path = CString::new(dir.as_os_str().as_bytes()).map_err(|_| None)?;
    open_dir_c(&c_path, libc::AT_FDCWD)
}

fn open_subdir(parent: &DirFd, name: &OsStr) -> Result<DirFd, Option<i32>> {
    let c_name = CString::new(name.as_bytes()).map_err(|_| None)?;
    open_dir_c(&c_name, parent.0)
}

fn open_dir_c(path: &CString, dirfd: libc::c_int) -> Result<DirFd, Option<i32>> {
    count_scanner_syscall();
    let fd = unsafe {
        libc::openat(
            dirfd,
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error());
    }
    Ok(DirFd(fd))
}

fn scan_open_dir(
    fd: &DirFd,
    dir: &Path,
    top_file_limit: usize,
    context: &ScanContext,
    cancellation: &ScanCancellation,
) -> DirectoryScan {
    scan_open_dir_with_reader(dir, top_file_limit, context, cancellation, |attrs, buf| {
        count_scanner_syscall();
        let n = unsafe {
            libc::getattrlistbulk(
                fd.0,
                attrs as *mut _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                FSOPT_RETURN_REALDEV,
            )
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n)
        }
    })
}

fn scan_open_dir_with_reader(
    dir: &Path,
    top_file_limit: usize,
    context: &ScanContext,
    cancellation: &ScanCancellation,
    mut read_batch: impl FnMut(&mut Attrlist, &mut [u8]) -> std::io::Result<i32>,
) -> DirectoryScan {
    if cancellation.is_cancelled() {
        return DirectoryScan::default();
    }

    let mut attrlist = Attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };
    thread_local! {
        static SCAN_BUF: std::cell::RefCell<Vec<u8>> =
            std::cell::RefCell::new(vec![0u8; 64 * 1024]);
    }
    let mut partial = DirectoryScan::default();
    let mut use_fallback = false;
    let mut saw_bulk_entries = false;

    SCAN_BUF.with(|cell| {
        let mut buf_ref = cell.borrow_mut();
        let buf = &mut *buf_ref;

        while !cancellation.is_cancelled() {
            let n = match read_batch(&mut attrlist, buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) => {
                    let errno = error.raw_os_error();
                    if errno == Some(libc::EINTR) {
                        continue;
                    }
                    if !saw_bulk_entries
                        && matches!(errno, Some(libc::ENOTSUP) | Some(libc::EINVAL))
                    {
                        use_fallback = true;
                        break;
                    }
                    partial.inaccessible = partial
                        .inaccessible
                        .saturating_add(inaccessible_from_errno(errno));
                    break;
                }
            };
            saw_bulk_entries = true;

            partial.merge(
                parse_bulk_batch(buf, n, dir, top_file_limit, context),
                top_file_limit,
            );
        }
    });

    if use_fallback && !cancellation.is_cancelled() {
        scan_open_dir_fallback(dir, top_file_limit, context, cancellation)
    } else {
        partial
    }
}

fn parse_bulk_batch(
    buf: &[u8],
    n: i32,
    dir: &Path,
    top_file_limit: usize,
    context: &ScanContext,
) -> DirectoryScan {
    let mut partial = DirectoryScan::default();
    let mut offset: usize = 0;
    for _ in 0..n {
        macro_rules! malformed_entry {
            () => {{
                partial.inaccessible = partial.inaccessible.saturating_add(1);
                break;
            }};
        }
        let entry_start = offset;
        if entry_start + 4 + ATTRIBUTE_SET_LEN > buf.len() {
            malformed_entry!();
        }
        let Some(length) = read_u32(buf, buf.len(), entry_start).map(|n| n as usize) else {
            malformed_entry!();
        };
        if length == 0 || entry_start + length > buf.len() {
            malformed_entry!();
        }
        let entry_end = entry_start + length;
        let Some(returned_common) = read_u32(buf, entry_end, entry_start + 4) else {
            malformed_entry!();
        };
        let Some(returned_file) = read_u32(buf, entry_end, entry_start + 16) else {
            malformed_entry!();
        };
        let mut field = entry_start + 4 + ATTRIBUTE_SET_LEN;

        let err = if returned_common & ATTR_CMN_ERROR != 0 {
            let Some(err) = read_u32(buf, entry_end, field) else {
                malformed_entry!();
            };
            field += 4;
            err
        } else {
            0
        };

        let name_ref = if returned_common & ATTR_CMN_NAME != 0 {
            let attr_ref_start = field;
            let Some(name_off) = read_i32(buf, entry_end, field).map(|n| n as isize) else {
                malformed_entry!();
            };
            let Some(name_len) = read_u32(buf, entry_end, field + 4).map(|n| n as usize) else {
                malformed_entry!();
            };
            field += ATTR_REFERENCE_LEN;
            Some((attr_ref_start, name_off, name_len))
        } else {
            None
        };

        let devid = if returned_common & ATTR_CMN_DEVID != 0 {
            let Some(devid) = read_i32(buf, entry_end, field) else {
                malformed_entry!();
            };
            field += 4;
            Some(devid as u32 as u64)
        } else {
            None
        };

        let objtype = if returned_common & ATTR_CMN_OBJTYPE != 0 {
            let Some(objtype) = read_u32(buf, entry_end, field) else {
                malformed_entry!();
            };
            field += 4;
            objtype
        } else {
            0
        };
        let fileid = if returned_common & ATTR_CMN_FILEID != 0 {
            let value = read_u64(buf, entry_end, field);
            field += 8;
            value
        } else {
            None
        };

        let linkcount = if returned_file & ATTR_FILE_LINKCOUNT != 0 {
            let Some(value) = read_u32(buf, entry_end, field) else {
                malformed_entry!();
            };
            field += 4;
            value
        } else {
            1
        };
        let totalsize = if returned_file & ATTR_FILE_TOTALSIZE != 0 {
            let Some(value) = read_u64(buf, entry_end, field) else {
                malformed_entry!();
            };
            field += 8;
            value
        } else {
            0
        };
        let allocsize = if returned_file & ATTR_FILE_ALLOCSIZE != 0 {
            let Some(value) = read_u64(buf, entry_end, field) else {
                malformed_entry!();
            };
            value
        } else {
            0
        };
        offset += length;

        // A per-entry error means the kernel could not read this
        // entry's attributes (transient race, ACL, or unsupported
        // attribute). Count it so the total stays an honest lower bound
        // instead of silently dropping the entry's bytes.
        if err != 0 {
            partial.inaccessible = partial.inaccessible.saturating_add(1);
            continue;
        }
        match objtype {
            VREG => {
                if returned_file & (ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE)
                    != (ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE)
                    || (linkcount > 1 && (devid.is_none() || fileid.is_none()))
                {
                    partial.inaccessible = partial.inaccessible.saturating_add(1);
                    continue;
                }
                let size = SizeInfo::new(totalsize, allocsize);
                if !should_count_file(devid, fileid, linkcount, context, size) {
                    continue;
                }
                partial.size.add_file(size);
                if top_file_limit == 0 {
                    continue;
                }
                if let Some((attr_ref_start, name_off, name_len)) = name_ref {
                    let Some(name_bytes) =
                        read_attr_reference(buf, entry_end, attr_ref_start, name_off, name_len)
                    else {
                        partial.inaccessible = partial.inaccessible.saturating_add(1);
                        continue;
                    };
                    push_largest_file_lazy(
                        &mut partial.largest_files,
                        top_file_limit,
                        size,
                        || dir.join(OsStr::from_bytes(name_bytes)),
                    );
                }
            }
            VDIR => {
                let Some((attr_ref_start, name_off, name_len)) = name_ref else {
                    partial.inaccessible = partial.inaccessible.saturating_add(1);
                    continue;
                };
                let Some(name_bytes) =
                    read_attr_reference(buf, entry_end, attr_ref_start, name_off, name_len)
                else {
                    partial.inaccessible = partial.inaccessible.saturating_add(1);
                    continue;
                };
                if matches!(name_bytes, b"." | b"..") {
                    continue;
                }
                let subdir = dir.join(OsStr::from_bytes(name_bytes));
                if should_skip_subdir(&subdir, devid, &context.policy) {
                    partial.skipped_mounts = partial.skipped_mounts.saturating_add(1);
                    continue;
                }
                partial.subdirs.push(subdir);
            }
            0 => {
                partial.inaccessible = partial.inaccessible.saturating_add(1);
            }
            _ => {} // VLNK, VSOCK, VBLK, VCHR, VFIFO: ignore
        }
    }
    partial
}

/// Compatibility path for filesystems that do not implement
/// `getattrlistbulk(2)`. This is intentionally selected only when the first
/// bulk call returns ENOTSUP/EINVAL, before any entries could be counted.
fn scan_open_dir_fallback(
    dir: &Path,
    top_file_limit: usize,
    context: &ScanContext,
    cancellation: &ScanCancellation,
) -> DirectoryScan {
    let mut partial = DirectoryScan::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            partial.inaccessible = inaccessible_from_errno(err.raw_os_error());
            return partial;
        }
    };

    for entry in entries {
        if cancellation.is_cancelled() {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                partial.inaccessible = partial
                    .inaccessible
                    .saturating_add(inaccessible_from_errno(err.raw_os_error()));
                continue;
            }
        };
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                partial.inaccessible = partial
                    .inaccessible
                    .saturating_add(inaccessible_from_errno(err.raw_os_error()));
                continue;
            }
        };

        if metadata.is_file() {
            let size = SizeInfo::new(metadata.len(), metadata.blocks().saturating_mul(512));
            if !should_count_file(
                Some(metadata.dev()),
                Some(metadata.ino()),
                metadata.nlink() as u32,
                context,
                size,
            ) {
                continue;
            }
            partial.size.add_file(size);
            if top_file_limit > 0 {
                push_largest_file(&mut partial.largest_files, top_file_limit, path, size);
            }
        } else if metadata.is_dir() {
            if should_skip_subdir(&path, Some(metadata.dev()), &context.policy) {
                partial.skipped_mounts = partial.skipped_mounts.saturating_add(1);
            } else {
                partial.subdirs.push(path);
            }
        }
    }

    partial
}

fn should_count_file(
    devid: Option<u64>,
    fileid: Option<u64>,
    linkcount: u32,
    context: &ScanContext,
    size: SizeInfo,
) -> bool {
    if linkcount <= 1 {
        return true;
    }
    let Some(fileid) = fileid else {
        return true;
    };
    let identity = (devid.unwrap_or(context.policy.root_dev), fileid);
    let mut seen = context
        .seen_hard_links
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let std::collections::hash_map::Entry::Vacant(entry) = seen.entry(identity) {
        entry.insert(size);
        true
    } else {
        false
    }
}

/// A subdirectory is skipped when it lives on a different volume than the scan
/// root. The root volume's device and (on modern macOS) the firmlinked Data
/// volume's device both belong to `/`, so both are counted; any other device
/// is a separate mount — an external/network/FUSE volume under `/Volumes` or a
/// custom path, or an APFS helper volume such as Preboot/VM/Update under
/// `/System/Volumes`. Entries whose device is unknown are kept (counted) rather
/// than guessed away.
fn should_skip_subdir(path: &Path, devid: Option<u64>, policy: &ScanPolicy) -> bool {
    if policy.skip_data_volume && path == Path::new(DATA_VOLUME_ROOT) {
        return true;
    }
    match devid {
        Some(dev) => dev != policy.root_dev && policy.data_dev != Some(dev),
        None => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FileCandidate {
    allocated: u64,
    logical: u64,
    path: PathBuf,
}

fn candidate_may_enter(
    heap: &BinaryHeap<Reverse<FileCandidate>>,
    limit: usize,
    size: SizeInfo,
) -> bool {
    limit > 0
        && (heap.len() < limit
            || heap.peek().is_none_or(|Reverse(smallest)| {
                (size.allocated, size.logical) >= (smallest.allocated, smallest.logical)
            }))
}

fn push_largest_file_lazy(
    heap: &mut BinaryHeap<Reverse<FileCandidate>>,
    limit: usize,
    size: SizeInfo,
    path: impl FnOnce() -> PathBuf,
) {
    if candidate_may_enter(heap, limit, size) {
        push_largest_file(heap, limit, path(), size);
    }
}

fn push_largest_file(
    heap: &mut BinaryHeap<Reverse<FileCandidate>>,
    limit: usize,
    path: PathBuf,
    size: SizeInfo,
) {
    if limit == 0 {
        return;
    }

    push_file_candidate(
        heap,
        limit,
        FileCandidate {
            allocated: size.allocated,
            logical: size.logical,
            path,
        },
    );
}

fn push_file_candidate(
    heap: &mut BinaryHeap<Reverse<FileCandidate>>,
    limit: usize,
    candidate: FileCandidate,
) {
    if limit == 0 {
        return;
    }

    if heap.len() < limit {
        heap.push(Reverse(candidate));
    } else if heap
        .peek()
        .map(|Reverse(smallest)| candidate > *smallest)
        .unwrap_or(false)
    {
        heap.pop();
        heap.push(Reverse(candidate));
    }
}

fn sorted_largest_files(heap: BinaryHeap<Reverse<FileCandidate>>) -> Vec<LargeFile> {
    let mut files: Vec<LargeFile> = heap
        .into_iter()
        .map(|Reverse(file)| LargeFile {
            path: file.path,
            size: SizeInfo::new(file.logical, file.allocated),
        })
        .collect();
    files.sort_by(|a, b| {
        b.size
            .allocated
            .cmp(&a.size.allocated)
            .then(b.size.logical.cmp(&a.size.logical))
            .then(a.path.cmp(&b.path))
    });
    files
}

#[inline]
fn read_u32(buf: &[u8], limit: usize, off: usize) -> Option<u32> {
    if off + 4 <= limit && off + 4 <= buf.len() {
        Some(u32::from_ne_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
        ]))
    } else {
        None
    }
}

#[inline]
fn read_i32(buf: &[u8], limit: usize, off: usize) -> Option<i32> {
    read_u32(buf, limit, off).map(|n| n as i32)
}

#[inline]
fn read_u64(buf: &[u8], limit: usize, off: usize) -> Option<u64> {
    if off + 8 <= limit && off + 8 <= buf.len() {
        Some(u64::from_ne_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
            buf[off + 4],
            buf[off + 5],
            buf[off + 6],
            buf[off + 7],
        ]))
    } else {
        None
    }
}

fn read_attr_reference(
    buf: &[u8],
    limit: usize,
    attr_ref_start: usize,
    data_offset: isize,
    data_len: usize,
) -> Option<&[u8]> {
    if data_len == 0 {
        return None;
    }
    let data_start = attr_ref_start.checked_add_signed(data_offset)?;
    if data_start + data_len > limit || data_start + data_len > buf.len() {
        return None;
    }
    let raw = &buf[data_start..data_start + data_len];
    let bytes = raw.strip_suffix(&[0u8]).unwrap_or(raw);
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct BoundaryFixture {
        root: PathBuf,
        logical: u64,
        allocated: u64,
    }
    impl BoundaryFixture {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root = test_root("filesystem_boundaries");
            std::fs::create_dir_all(&root).unwrap();
            let mut files = Vec::new();
            let mut dir = root.join("deep");
            for _ in 0..64 {
                std::fs::create_dir_all(&dir).unwrap();
                let file = dir.join("file");
                std::fs::write(&file, b"x").unwrap();
                files.push(file);
                dir = dir.join("d");
            }
            let original = root.join("original");
            std::fs::write(&original, b"fixture").unwrap();
            std::fs::hard_link(&original, root.join("hardlink")).unwrap();
            files.push(original);
            let sparse = root.join("sparse");
            std::fs::File::create(&sparse)
                .unwrap()
                .set_len(1 << 30)
                .unwrap();
            assert!(std::fs::metadata(&sparse).unwrap().blocks() * 512 < 1 << 30);
            files.push(sparse);
            let source = root.join("clone-source");
            let clone = root.join("clone");
            std::fs::write(&source, vec![1u8; 8192]).unwrap();
            let source_c = CString::new(source.as_os_str().as_bytes()).unwrap();
            let clone_c = CString::new(clone.as_os_str().as_bytes()).unwrap();
            if unsafe { libc::clonefile(source_c.as_ptr(), clone_c.as_ptr(), 0) } == 0 {
                files.push(clone);
            } else {
                let error = std::io::Error::last_os_error();
                assert_eq!(error.raw_os_error(), Some(libc::ENOTSUP));
                eprintln!("clone fixture unavailable on this filesystem: {error}");
            }
            files.push(source);
            let locked = root.join("locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::write(locked.join("unreadable"), b"hidden").unwrap();
            std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let (logical, allocated) = files
                .iter()
                .map(|path| std::fs::metadata(path).unwrap())
                .fold((0, 0), |(logical, allocated), meta| {
                    (logical + meta.len(), allocated + meta.blocks() * 512)
                });
            Self {
                root,
                logical,
                allocated,
            }
        }
    }
    impl Drop for BoundaryFixture {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                self.root.join("locked"),
                std::fs::Permissions::from_mode(0o755),
            );
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn deep_sparse_clone_and_hardlink_fixture_preserves_coverage_and_accounting() {
        let fixture = BoundaryFixture::new();
        let scan = scan_dir(&fixture.root, 10);
        assert_eq!(scan.inaccessible, 1);
        assert_eq!(scan.size, SizeInfo::new(fixture.logical, fixture.allocated));
        assert_eq!(scan.skipped_mounts, 0);
    }

    #[test]
    #[ignore = "filesystem boundary performance protocol; run optimized"]
    fn benchmark_filesystem_boundaries() {
        let fixture = BoundaryFixture::new();
        let mut times = Vec::new();
        let _ = scan_dir(&fixture.root, 10);
        let mut calls = Vec::new();
        for _ in 0..12 {
            let before = SCANNER_SYSCALLS.load(Ordering::Relaxed);
            let started = std::time::Instant::now();
            let scan = scan_dir(&fixture.root, 10);
            times.push(started.elapsed());
            calls.push(SCANNER_SYSCALLS.load(Ordering::Relaxed) - before);
            assert_eq!(scan.size, SizeInfo::new(fixture.logical, fixture.allocated));
            assert_eq!(scan.inaccessible, 1);
        }
        times.sort_unstable();
        eprintln!(
            "boundary_fixture: median_us={} p95_us={} direct_scanner_syscalls={calls:?}",
            times[6].as_micros(),
            times[11].as_micros()
        );
    }

    fn shared_job_for(root: &Path) -> Arc<SharedWalk> {
        recover_lock(shared_registry())
            .iter()
            .find(|(key, _)| key.root == root)
            .and_then(|(_, job)| job.upgrade())
            .expect("registered shared walk")
    }

    #[test]
    #[ignore = "on-disk scan and path-construction performance protocol"]
    fn benchmark_shared_scan_and_heap_paths() {
        use std::time::Instant;
        let root = test_root("scan_performance");
        std::fs::create_dir_all(&root).unwrap();
        for directory in 0..100 {
            let child = root.join(format!("directory-{directory:03}"));
            std::fs::create_dir(&child).unwrap();
            for file in 0..100 {
                std::fs::write(child.join(format!("file-{file:03}")), b"fixture").unwrap();
            }
        }
        let run_raw = |root: PathBuf| {
            let (tx, rx) = std::sync::mpsc::channel();
            scan_dir_async_unshared(
                root,
                0,
                ScanCancellation::never(),
                None,
                || {},
                move |scan| {
                    tx.send(scan.unwrap()).unwrap();
                },
            );
            rx.recv_timeout(Duration::from_secs(30)).unwrap()
        };
        let expected = run_raw(root.clone());
        assert_eq!(expected.size.logical, 70_000);
        let mut raw_times = Vec::new();
        let mut broker_times = Vec::new();
        let mut raw_overlap_times = Vec::new();
        let mut shared_overlap_times = Vec::new();
        for _ in 0..12 {
            let started = Instant::now();
            assert_eq!(run_raw(root.clone()), expected);
            raw_times.push(started.elapsed());
            let started = Instant::now();
            assert_eq!(scan_dir(&root, 0), expected);
            broker_times.push(started.elapsed());

            let started = Instant::now();
            std::thread::scope(|scope| {
                let workers: Vec<_> = (0..4)
                    .map(|_| scope.spawn(|| run_raw(root.clone())))
                    .collect();
                for worker in workers {
                    assert_eq!(worker.join().unwrap(), expected);
                }
            });
            raw_overlap_times.push(started.elapsed());

            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (tx, rx) = std::sync::mpsc::channel();
            let started = Instant::now();
            let first_tx = tx.clone();
            shared_scan_async(
                root.clone(),
                0,
                ScanCancellation::never(),
                None,
                false,
                move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                },
                move |scan| {
                    first_tx.send(scan.unwrap()).unwrap();
                },
            );
            started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            for _ in 0..3 {
                let tx = tx.clone();
                shared_scan_async(
                    root.clone(),
                    0,
                    ScanCancellation::never(),
                    None,
                    false,
                    || {},
                    move |scan| {
                        tx.send(scan.unwrap()).unwrap();
                    },
                );
            }
            assert_eq!(recover_lock(&shared_job_for(&root).clients).len(), 4);
            release_tx.send(()).unwrap();
            for _ in 0..4 {
                assert_eq!(rx.recv_timeout(Duration::from_secs(30)).unwrap(), expected);
            }
            shared_overlap_times.push(started.elapsed());
        }
        for (label, mut times) in [
            ("raw_single", raw_times),
            ("broker_single", broker_times),
            ("raw_four", raw_overlap_times),
            ("shared_four", shared_overlap_times),
        ] {
            times.sort_unstable();
            eprintln!(
                "{label}: median_us={} p95_us={} samples={}",
                times[times.len() / 2].as_micros(),
                times[times.len() - 1].as_micros(),
                times.len()
            );
        }

        let parent = root.join("long-path-component-for-a-rejected-small-file");
        for lazy in [false, true] {
            let mut times = Vec::new();
            for _ in 0..12 {
                let mut heap = BinaryHeap::new();
                push_largest_file(&mut heap, 1, root.join("largest"), SizeInfo::new(100, 8192));
                let started = Instant::now();
                for _ in 0..200_000 {
                    let size = std::hint::black_box(SizeInfo::new(1, 4096));
                    if lazy {
                        push_largest_file_lazy(&mut heap, 1, size, || parent.join("file"));
                    } else {
                        push_largest_file(&mut heap, 1, parent.join("file"), size);
                    }
                }
                times.push(started.elapsed());
                assert_eq!(heap.len(), 1);
            }
            times.sort_unstable();
            eprintln!(
                "heap_lazy={lazy}: median_us={} p95_us={}",
                times[6].as_micros(),
                times[11].as_micros()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_scans_share_one_walk_but_cancel_independently() {
        let root = test_root("shared_scan_clients");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), b"fixture").unwrap();
        let generation = Arc::new(AtomicU64::new(0));
        let first = ScanCancellation::new(Arc::clone(&generation), 0);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (first_tx, first_rx) = std::sync::mpsc::channel();
        shared_scan_async(
            root.clone(),
            0,
            first,
            None,
            false,
            move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            },
            move |result| {
                first_tx.send(result).unwrap();
            },
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let initial_job = shared_job_for(&root);
        let (second_tx, second_rx) = std::sync::mpsc::channel();
        shared_scan_async(
            root.clone(),
            0,
            ScanCancellation::never(),
            None,
            true,
            || {},
            move |result| {
                second_tx.send(result).unwrap();
            },
        );
        assert!(Arc::ptr_eq(&initial_job, &shared_job_for(&root)));
        assert_eq!(recover_lock(&initial_job.clients).len(), 2);
        assert!(initial_job.interactive.load(Ordering::Relaxed));
        generation.fetch_add(1, Ordering::Relaxed);
        release_tx.send(()).unwrap();
        assert!(first_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_none());
        assert_eq!(
            second_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .size
                .logical,
            7
        );
        assert!(!recover_lock(shared_registry())
            .keys()
            .any(|key| key.root == root));
        std::fs::write(root.join("file"), b"longer fixture").unwrap();
        assert_eq!(scan_dir(&root, 0).size.logical, 14);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_scan_keys_separate_policy_limits_and_invalidated_work() {
        let root = test_root("shared_scan_policy");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), b"fixture").unwrap();
        let device = std::fs::symlink_metadata(&root).unwrap().dev();
        let policy = ScanPolicy::new(&root, device);
        let mut alternate_policy = policy;
        alternate_policy.skip_data_volume = !policy.skip_data_volume;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        for (limit, policy) in [(0, policy), (1, policy), (0, alternate_policy)] {
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            releases.push(release_tx);
            let done_tx = done_tx.clone();
            shared_scan_async(
                root.clone(),
                limit,
                ScanCancellation::never(),
                Some(policy),
                false,
                move || {
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                },
                move |result| {
                    done_tx.send(result).unwrap();
                },
            );
        }
        assert_eq!(
            recover_lock(shared_registry())
                .keys()
                .filter(|key| key.root == root)
                .count(),
            3
        );
        let old_jobs: Vec<_> = recover_lock(shared_registry())
            .iter()
            .filter(|(key, _)| key.root == root)
            .filter_map(|(_, job)| job.upgrade())
            .collect();
        invalidate_shared_scans(&root.join("file"));
        assert!(!recover_lock(shared_registry())
            .keys()
            .any(|key| key.root == root));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        releases.push(release_tx);
        shared_scan_async(
            root.clone(),
            0,
            ScanCancellation::never(),
            Some(policy),
            true,
            move || {
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            },
            move |result| {
                done_tx.send(result).unwrap();
            },
        );
        let replacement = shared_job_for(&root);
        assert!(old_jobs.iter().all(|old| !Arc::ptr_eq(old, &replacement)));
        for release in releases {
            release.send(()).unwrap();
        }
        for _ in 0..4 {
            assert_eq!(
                done_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap()
                    .size
                    .logical,
                7
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_job_cannot_absorb_or_remove_a_new_live_request() {
        let root = test_root("shared_scan_replace_cancelled");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), b"fixture").unwrap();
        let generation = Arc::new(AtomicU64::new(0));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_old_tx, release_old_rx) = std::sync::mpsc::channel();
        let (old_tx, old_rx) = std::sync::mpsc::channel();
        shared_scan_async(
            root.clone(),
            0,
            ScanCancellation::new(Arc::clone(&generation), 0),
            None,
            false,
            move || {
                started_tx.send(()).unwrap();
                release_old_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            },
            move |result| {
                old_tx.send(result).unwrap();
            },
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let old = shared_job_for(&root);
        generation.fetch_add(1, Ordering::Relaxed);
        let (release_new_tx, release_new_rx) = std::sync::mpsc::channel();
        let (new_tx, new_rx) = std::sync::mpsc::channel();
        shared_scan_async(
            root.clone(),
            0,
            ScanCancellation::never(),
            None,
            true,
            move || {
                release_new_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            },
            move |result| {
                new_tx.send(result).unwrap();
            },
        );
        let new = shared_job_for(&root);
        assert!(!Arc::ptr_eq(&old, &new));
        release_old_tx.send(()).unwrap();
        assert!(old_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_none());
        assert!(Arc::ptr_eq(&new, &shared_job_for(&root)));
        release_new_tx.send(()).unwrap();
        assert_eq!(
            new_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .size
                .logical,
            7
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_subscriber_orders_start_before_done_even_when_completion_races() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let start_events = Arc::clone(&events);
        let done_events = Arc::clone(&events);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let subscriber = Arc::new(ScanSubscriber {
            cancellation: ScanCancellation::never(),
            start: Mutex::new(Some(Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                recover_lock(&start_events).push("start");
            }))),
            done: Mutex::new(Some(Box::new(move |_| {
                recover_lock(&done_events).push("done");
            }))),
        });
        let start = Arc::clone(&subscriber);
        let start_thread = std::thread::spawn(move || start.start());
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let done_thread = std::thread::spawn(move || subscriber.finish(&Some(DirScan::default())));
        release_tx.send(()).unwrap();
        start_thread.join().unwrap();
        done_thread.join().unwrap();
        assert_eq!(*recover_lock(&events), ["start", "done"]);
    }

    #[test]
    fn heap_rejection_skips_smaller_sizes_but_keeps_path_ties_comparable() {
        let mut heap = BinaryHeap::new();
        push_largest_file(
            &mut heap,
            1,
            PathBuf::from("first"),
            SizeInfo::new(12, 4096),
        );
        assert!(!candidate_may_enter(&heap, 1, SizeInfo::new(8, 4096)));
        assert!(!candidate_may_enter(&heap, 1, SizeInfo::new(100, 0)));
        assert!(candidate_may_enter(&heap, 1, SizeInfo::new(12, 4096)));
        assert!(candidate_may_enter(&heap, 1, SizeInfo::new(8, 8192)));
        assert!(!candidate_may_enter(&heap, 0, SizeInfo::new(8, 8192)));
        let builds = std::cell::Cell::new(0);
        for _ in 0..10_000 {
            push_largest_file_lazy(&mut heap, 1, SizeInfo::new(8, 4096), || {
                builds.set(builds.get() + 1);
                PathBuf::from("rejected")
            });
        }
        assert_eq!(builds.get(), 0);
        push_largest_file_lazy(&mut heap, 1, SizeInfo::new(12, 4096), || {
            builds.set(builds.get() + 1);
            PathBuf::from("path tie")
        });
        assert_eq!(builds.get(), 1);
    }
    use std::fs;
    use std::os::unix::fs::symlink;

    fn packed_file_record(with_total: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; 24];
        let common = ATTR_CMN_DEVID | ATTR_CMN_OBJTYPE | ATTR_CMN_FILEID;
        let file = ATTR_FILE_LINKCOUNT
            | ATTR_FILE_ALLOCSIZE
            | if with_total { ATTR_FILE_TOTALSIZE } else { 0 };
        bytes[4..8].copy_from_slice(&common.to_ne_bytes());
        bytes[16..20].copy_from_slice(&file.to_ne_bytes());
        bytes.extend(5i32.to_ne_bytes());
        bytes.extend(VREG.to_ne_bytes());
        bytes.extend(7u64.to_ne_bytes());
        bytes.extend(1u32.to_ne_bytes());
        if with_total {
            bytes.extend(123u64.to_ne_bytes());
        }
        bytes.extend(4096u64.to_ne_bytes());
        let len = bytes.len() as u32;
        bytes[..4].copy_from_slice(&len.to_ne_bytes());
        bytes
    }

    #[test]
    fn variable_layout_keeps_fields_aligned_when_attributes_are_absent() {
        let context = ScanContext {
            policy: ScanPolicy::with_devices(Path::new("/tmp"), 5, None),
            seen_hard_links: Mutex::new(HashMap::new()),
        };
        let mut bytes = packed_file_record(true);
        bytes.extend(packed_file_record(false));
        let scan = parse_bulk_batch(&bytes, 2, Path::new("/tmp"), 0, &context);
        assert_eq!(scan.size, SizeInfo::new(123, 4096));
        assert_eq!(scan.inaccessible, 1);
    }

    #[test]
    fn opened_descriptor_policy_rejects_a_foreign_device_before_reading_files() {
        let root = test_root("descriptor_policy");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data"), b"data").unwrap();
        let dev = fs::metadata(&root).unwrap().dev();
        let state = Arc::new(WalkState::new(
            ScanPolicy::with_devices(&root, dev.wrapping_add(1), None),
            0,
            ScanCancellation::never(),
        ));
        let group = TaskGroup::new();
        spawn_walk(root.clone(), Arc::clone(&state), &group);
        group.wait();
        let result = state.finish();
        assert_eq!(result.skipped_mounts, 1);
        assert_eq!(result.size.logical, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn actual_apfs_helper_mount_obeys_the_selected_root_policy() {
        let root = std::env::temp_dir();
        let scope = ScanScope::new(&root).unwrap();
        for mount in ["/System/Volumes/VM", "/System/Volumes/Preboot"] {
            let path = Path::new(mount);
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                continue;
            };
            if scope.accepts(path, &meta) {
                continue;
            }
            // The descriptor is checked before enumeration: no VM or user files are read.
            let scan =
                scan_dir_in_scope(path, 0, &ScanCancellation::never(), Some(&scope)).unwrap();
            if scan.inaccessible == 0 {
                assert_eq!(scan.skipped_mounts, 1);
                assert_eq!(scan.size, SizeInfo::default());
            }
        }
    }

    #[test]
    fn matches_stat_sum_on_known_tree() {
        let root = test_root("bulkstat");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("root.txt"), b"hello world\n").unwrap(); // 12
        fs::write(root.join("a/big.bin"), vec![0u8; 1024 * 1000]).unwrap(); // 1_024_000
        fs::write(root.join("a/b/c/deep.txt"), b"nested\n").unwrap(); // 7
        let _ = symlink("/nonexistent", root.join("broken-link"));

        let got = scan_dir(&root, 0).size.logical;
        assert_eq!(got, 12 + 1_024_000 + 7, "bulkstat size mismatch");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scan_reports_allocated_size_and_largest_files() {
        let root = test_root("bulkstat_scan");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("small.txt"), b"small").unwrap();
        fs::write(root.join("a/large.bin"), vec![0u8; 4096 * 8]).unwrap();
        fs::write(root.join("a/b/medium.bin"), vec![0u8; 4096]).unwrap();

        let scan = scan_dir(&root, 2);

        assert_eq!(scan.size.logical, 5 + 4096 * 8 + 4096);
        assert!(scan.size.allocated > 0);
        assert_eq!(scan.largest_files.len(), 2);
        assert_eq!(
            scan.largest_files[0].path.canonicalize().unwrap(),
            root.join("a/large.bin").canonicalize().unwrap()
        );
        assert_eq!(scan.largest_files[0].size.logical, 4096 * 8);
        assert_eq!(
            scan.largest_files[1].path.canonicalize().unwrap(),
            root.join("a/b/medium.bin").canonicalize().unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_subdir_is_counted_and_rest_still_sums() {
        use std::os::unix::fs::PermissionsExt;

        struct RestorePerms(PathBuf);
        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
                let _ = fs::remove_dir_all(self.0.parent().unwrap());
            }
        }

        let root = test_root("bulkstat_perm");
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(root.join("readable.txt"), b"0123456789").unwrap(); // 10
        fs::write(locked.join("hidden.bin"), vec![0u8; 4096]).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let _guard = RestorePerms(locked.clone());

        let scan = scan_dir(&root, 0);
        assert_eq!(scan.inaccessible, 1, "locked dir should be counted");
        assert_eq!(scan.size.logical, 10, "readable part should still sum");
    }

    #[test]
    fn hard_linked_file_counts_once() {
        let root = test_root("bulkstat_hardlink");
        let original = root.join("original.bin");
        let linked = root.join("linked.bin");
        fs::create_dir_all(&root).unwrap();
        fs::write(&original, vec![1u8; 4096]).unwrap();
        fs::hard_link(&original, &linked).unwrap();

        let scan = scan_dir(&root, 10);

        assert_eq!(scan.size.logical, 4096, "hard link should not double count");
        assert_eq!(scan.largest_files.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fallback_scanner_preserves_sizes_subdirs_and_hard_link_deduplication() {
        let root = test_root("bulkstat_fallback");
        fs::create_dir_all(root.join("child")).unwrap();
        fs::write(root.join("data.bin"), vec![1u8; 4096]).unwrap();
        fs::hard_link(root.join("data.bin"), root.join("data-link.bin")).unwrap();
        let metadata = fs::metadata(&root).unwrap();
        let context = ScanContext {
            policy: ScanPolicy::with_devices(&root, metadata.dev(), None),
            seen_hard_links: Mutex::new(HashMap::new()),
        };

        let scan = scan_open_dir_fallback(&root, 10, &context, &ScanCancellation::never());

        assert_eq!(scan.size.logical, 4096);
        assert_eq!(scan.largest_files.len(), 1);
        assert_eq!(scan.subdirs, vec![root.join("child")]);
        assert_eq!(scan.inaccessible, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_directory_counts_as_zero() {
        let root = test_root("missing");
        assert_eq!(scan_dir(&root, 0).size.logical, 0);
    }

    #[test]
    fn root_symlink_to_directory_counts_as_zero() {
        let root = test_root("symlink_root");
        let target = root.join("target");
        let link = root.join("link");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("data.bin"), vec![1u8; 4096]).unwrap();
        symlink(&target, &link).unwrap();

        assert_eq!(scan_dir(&link, 0).size.logical, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn enametoolong_counts_as_inaccessible() {
        // A spawned sibling whose absolute path exceeds the platform limit
        // fails to reopen with ENAMETOOLONG; it must be surfaced as
        // inaccessible (a lower-bound flag), never silently dropped to zero.
        assert_eq!(inaccessible_from_errno(Some(libc::ENAMETOOLONG)), 1);
        assert_eq!(inaccessible_from_errno(Some(libc::EACCES)), 1);
        assert_eq!(inaccessible_from_errno(Some(libc::EPERM)), 1);
        assert_eq!(inaccessible_from_errno(Some(libc::EIO)), 1);
        assert_eq!(inaccessible_from_errno(Some(libc::EMFILE)), 1);
        assert_eq!(inaccessible_from_errno(Some(libc::ENOENT)), 0);
        assert_eq!(inaccessible_from_errno(None), 1);
    }

    #[test]
    fn packed_attribute_readers_reject_arbitrary_out_of_bounds_ranges() {
        let mut seed = 0x9e37_79b9_u64;
        for len in 0..256usize {
            let mut bytes = vec![0u8; len];
            for byte in &mut bytes {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                *byte = (seed >> 32) as u8;
            }
            for _ in 0..32 {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let limit = (seed as usize) % 320;
                let offset = ((seed >> 16) as usize) % 320;
                let data_len = ((seed >> 32) as usize) % 320;
                let signed_offset = ((seed >> 48) as i16) as isize;
                let _ = read_u32(&bytes, limit, offset);
                let _ = read_u64(&bytes, limit, offset);
                if let Some(slice) =
                    read_attr_reference(&bytes, limit, offset, signed_offset, data_len)
                {
                    assert!(slice.as_ptr() >= bytes.as_ptr());
                    assert!(slice.as_ptr_range().end <= bytes.as_ptr_range().end);
                    assert!(!slice.is_empty());
                }
            }
        }
    }

    #[test]
    fn skip_policy_skips_data_volume_only_from_ancestor() {
        let root_policy = ScanPolicy::with_devices(Path::new("/"), 10, Some(11));
        assert!(should_skip_subdir(
            Path::new(DATA_VOLUME_ROOT),
            Some(11),
            &root_policy
        ));

        let data_policy = ScanPolicy::with_devices(Path::new(DATA_VOLUME_ROOT), 11, Some(11));
        assert!(!should_skip_subdir(
            Path::new(DATA_VOLUME_ROOT),
            Some(11),
            &data_policy
        ));
    }

    #[test]
    fn skip_policy_allows_root_and_data_devices_skips_other_volumes() {
        // Scanning `/`: root volume is dev 10, firmlinked Data volume is dev 11.
        let policy = ScanPolicy::with_devices(Path::new("/"), 10, Some(11));

        // Firmlinked Data-volume content (home dir) is on data_dev: counted.
        assert!(!should_skip_subdir(
            Path::new("/Users/example"),
            Some(11),
            &policy
        ));
        // Same device as the root volume: counted.
        assert!(!should_skip_subdir(
            Path::new("/System/Library"),
            Some(10),
            &policy
        ));
        // External volume on its own device: skipped.
        assert!(should_skip_subdir(
            Path::new("/Volumes/External"),
            Some(20),
            &policy
        ));
        // APFS helper volumes are NOT under /Volumes but live on their own
        // devices; the old /Volumes-only policy counted them into `/`.
        assert!(should_skip_subdir(
            Path::new("/System/Volumes/VM"),
            Some(21),
            &policy
        ));
        assert!(should_skip_subdir(
            Path::new("/System/Volumes/Preboot"),
            Some(22),
            &policy
        ));
        // A network/FUSE mount at a custom path is skipped too.
        assert!(should_skip_subdir(
            Path::new("/Users/example/nfs"),
            Some(23),
            &policy
        ));
        // Unknown device: kept rather than guessed away.
        assert!(!should_skip_subdir(
            Path::new("/Users/example/unknown"),
            None,
            &policy
        ));
    }

    #[test]
    fn cancelled_scan_returns_no_result() {
        let root = test_root("cancelled");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data.bin"), vec![1u8; 4096]).unwrap();
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(2));
        let cancellation = ScanCancellation::new(generation, 1);

        assert!(scan_dir_with_cancellation(&root, 0, &cancellation).is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_scan_cancellation_after_first_batch_stops_reads_and_suppresses_results() {
        let root = test_root("cancel_native_batch");
        let generation = Arc::new(AtomicU64::new(0));
        let cancellation = ScanCancellation::new(Arc::clone(&generation), 0);
        let state = WalkState::new(
            ScanPolicy::with_devices(&root, 5, None),
            0,
            cancellation.clone(),
        );
        let bytes = packed_file_record(true);
        let mut reads = 0;
        let partial =
            scan_open_dir_with_reader(&root, 0, &state.context, &cancellation, |_, buffer| {
                reads += 1;
                assert_eq!(reads, 1, "cancelled scan read another native batch");
                buffer[..bytes.len()].copy_from_slice(&bytes);
                generation.fetch_add(1, Ordering::Relaxed);
                Ok(1)
            });
        assert_eq!(reads, 1);
        assert_eq!(partial.size, SizeInfo::new(123, 4096));
        state.merge(partial);
        let (tx, rx) = std::sync::mpsc::channel();
        let subscriber = ScanSubscriber {
            cancellation,
            start: Mutex::new(None),
            done: Mutex::new(Some(Box::new(move |result| tx.send(result).unwrap()))),
        };
        subscriber.finish(&Some(state.finish()));
        assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap().is_none());
    }

    #[test]
    fn native_scan_cancellation_skips_interrupted_retries_and_fallback() {
        let root = test_root("cancel_native_error");
        fs::write(&root, b"not a directory").unwrap();
        for errno in [libc::EINTR, libc::ENOTSUP, libc::EINVAL] {
            let generation = Arc::new(AtomicU64::new(0));
            let cancellation = ScanCancellation::new(Arc::clone(&generation), 0);
            let context = ScanContext {
                policy: ScanPolicy::with_devices(&root, 5, None),
                seen_hard_links: Mutex::new(HashMap::new()),
            };
            let mut reads = 0;
            let partial = scan_open_dir_with_reader(&root, 0, &context, &cancellation, |_, _| {
                reads += 1;
                assert_eq!(reads, 1, "cancelled scan retried a native read");
                generation.fetch_add(1, Ordering::Relaxed);
                Err(std::io::Error::from_raw_os_error(errno))
            });
            assert_eq!(reads, 1);
            assert_eq!(partial.size, SizeInfo::default());
            assert_eq!(partial.inaccessible, 0, "cancelled scan entered fallback");
        }
        fs::remove_file(root).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
