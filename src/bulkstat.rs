//! macOS-native directory-size walker using `getattrlistbulk(2)`.
//!
//! Instead of `readdir` + one `stat` per file (the usual pattern), this issues a
//! single syscall that returns packed attributes for dozens of entries at once.
//! On directories like ~/Library or node_modules that have thousands of small
//! files, this is 3-10x faster than stat-per-file because each syscall has a
//! fixed kernel-mode overhead that dominates when files are small.
//!
//! Layout of each returned entry (with our attrlist + FSOPT_PACK_INVAL_ATTRS):
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
use std::collections::{BinaryHeap, HashSet};
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
const FSOPT_PACK_INVAL_ATTRS: u64 = 0x00000008;
const FSOPT_RETURN_REALDEV: u64 = 0x00000200;
const ATTRIBUTE_SET_LEN: usize = 20;
const ATTR_REFERENCE_LEN: usize = 8;
const DATA_VOLUME_ROOT: &str = "/System/Volumes/Data";

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
    /// Directories that could not be opened (EACCES/EPERM/ENAMETOOLONG) plus
    /// individual entries whose attributes could not be read. When non-zero the
    /// reported size is a lower bound.
    pub inaccessible: u32,
    /// Subdirectories skipped because they sit on a different volume than the
    /// scan root (external/network/FUSE mounts and APFS helper volumes such as
    /// Preboot/VM/Update), to avoid counting another filesystem's bytes.
    pub skipped_mounts: u32,
}

#[derive(Clone, Debug)]
pub struct ScanCancellation {
    generation: Arc<AtomicU64>,
    expected: u64,
}

impl ScanCancellation {
    pub fn new(generation: Arc<AtomicU64>, expected: u64) -> Self {
        Self {
            generation,
            expected,
        }
    }

    pub fn never() -> Self {
        Self::new(Arc::new(AtomicU64::new(0)), 0)
    }

    pub fn is_cancelled(&self) -> bool {
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
/// (EACCES/EPERM/ENAMETOOLONG) and individual entries whose attributes cannot
/// be read are counted in `inaccessible`; when it is non-zero the reported size
/// is a lower bound. Subdirectories on a different volume than the scan root
/// are skipped and counted in `skipped_mounts`.
pub fn scan_dir(root: &Path, top_file_limit: usize) -> DirScan {
    scan_dir_with_cancellation(root, top_file_limit, &ScanCancellation::never()).unwrap_or_default()
}

pub fn scan_dir_with_cancellation(
    root: &Path,
    top_file_limit: usize,
    cancellation: &ScanCancellation,
) -> Option<DirScan> {
    if cancellation.is_cancelled() {
        return None;
    }
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        return Some(DirScan::default());
    };
    if !meta.file_type().is_dir() {
        return Some(DirScan::default());
    }

    let policy_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let state = Arc::new(WalkState::new(
        ScanPolicy::new(&policy_root, meta.dev()),
        top_file_limit,
        cancellation.clone(),
    ));
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
    on_start: impl FnOnce() + Send + 'static,
    on_done: impl FnOnce(Option<DirScan>) + Send + 'static,
) {
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
    group.spawn(move || {
        if cancellation.is_cancelled() {
            return;
        }
        on_start();
        let Ok(meta) = std::fs::symlink_metadata(&root) else {
            return;
        };
        if !meta.file_type().is_dir() {
            return;
        }
        let policy_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let state = Arc::new(WalkState::new(
            ScanPolicy::new(&policy_root, meta.dev()),
            top_file_limit,
            cancellation.clone(),
        ));
        *result_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&state));
        walk_task(root, state, walk_group);
    });
    group.arm();
}

/// Shared accumulator for one walk. Sizes and counters are relaxed atomics
/// so completed directories merge without a global lock; the
/// largest-files heap keeps a mutex but is only touched when a top-files
/// report was requested.
struct WalkState {
    context: ScanContext,
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
            context: ScanContext {
                policy,
                seen_hard_links: Mutex::new(HashSet::new()),
            },
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

struct ScanContext {
    policy: ScanPolicy,
    seen_hard_links: Mutex<HashSet<FileIdentity>>,
}

type FileIdentity = (u64, u64);

#[derive(Clone, Copy)]
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
        unsafe { libc::close(self.0) };
    }
}

/// EACCES/EPERM means real data was skipped (no Full Disk Access, ACLs), and
/// ENAMETOOLONG means a spawned sibling branch's absolute path grew past the
/// platform limit so a whole subtree could not be reopened — both undercount
/// and must be surfaced rather than silently dropped. ENOENT is just a deletion
/// race during the walk and stays silent.
fn inaccessible_from_errno(errno: Option<i32>) -> u32 {
    u32::from(matches!(
        errno,
        Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::ENAMETOOLONG)
    ))
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
    let fd = unsafe {
        libc::openat(
            dirfd,
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
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
    if cancellation.is_cancelled() {
        return DirectoryScan::default();
    }
    let fd = fd.0;

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

    SCAN_BUF.with(|cell| {
        let mut buf_ref = cell.borrow_mut();
        let buf = &mut *buf_ref;

        loop {
            let n = unsafe {
                libc::getattrlistbulk(
                    fd,
                    &mut attrlist as *mut _ as *mut libc::c_void,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    FSOPT_PACK_INVAL_ATTRS | FSOPT_RETURN_REALDEV,
                )
            };
            if n <= 0 {
                break;
            }

            let mut offset: usize = 0;
            for _ in 0..n {
                let entry_start = offset;
                if entry_start + 4 + ATTRIBUTE_SET_LEN > buf.len() {
                    break;
                }
                let Some(length) = read_u32(buf, buf.len(), entry_start).map(|n| n as usize) else {
                    break;
                };
                if length == 0 || entry_start + length > buf.len() {
                    break;
                }
                let entry_end = entry_start + length;
                let Some(returned_common) = read_u32(buf, entry_end, entry_start + 4) else {
                    break;
                };
                let Some(returned_file) = read_u32(buf, entry_end, entry_start + 16) else {
                    break;
                };
                let mut field = entry_start + 4 + ATTRIBUTE_SET_LEN;

                let err = if returned_common & ATTR_CMN_ERROR != 0 {
                    let Some(err) = read_u32(buf, entry_end, field) else {
                        break;
                    };
                    field += 4;
                    err
                } else {
                    0
                };

                let name_ref = if returned_common & ATTR_CMN_NAME != 0 {
                    let attr_ref_start = field;
                    let Some(name_off) = read_i32(buf, entry_end, field).map(|n| n as isize) else {
                        break;
                    };
                    let Some(name_len) = read_u32(buf, entry_end, field + 4).map(|n| n as usize)
                    else {
                        break;
                    };
                    field += ATTR_REFERENCE_LEN;
                    Some((attr_ref_start, name_off, name_len))
                } else {
                    None
                };

                let devid = if returned_common & ATTR_CMN_DEVID != 0 {
                    let Some(devid) = read_i32(buf, entry_end, field) else {
                        break;
                    };
                    field += 4;
                    Some(devid as u32 as u64)
                } else {
                    None
                };

                let objtype = if returned_common & ATTR_CMN_OBJTYPE != 0 {
                    let Some(objtype) = read_u32(buf, entry_end, field) else {
                        break;
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
                        break;
                    };
                    field += 4;
                    value
                } else {
                    1
                };
                let totalsize = if returned_file & ATTR_FILE_TOTALSIZE != 0 {
                    let value = read_u64(buf, entry_end, field).unwrap_or(0);
                    field += 8;
                    value
                } else {
                    0
                };
                let allocsize = if returned_file & ATTR_FILE_ALLOCSIZE != 0 {
                    read_u64(buf, entry_end, field).unwrap_or(0)
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
                        if !should_count_file(devid, fileid, linkcount, context) {
                            continue;
                        }
                        let size = SizeInfo::new(totalsize, allocsize);
                        partial.size.add_file(size);
                        if top_file_limit == 0 {
                            continue;
                        }
                        if let Some((attr_ref_start, name_off, name_len)) = name_ref {
                            let Some(name_bytes) = read_attr_reference(
                                buf,
                                entry_end,
                                attr_ref_start,
                                name_off,
                                name_len,
                            ) else {
                                continue;
                            };
                            push_largest_file(
                                &mut partial.largest_files,
                                top_file_limit,
                                dir.join(OsStr::from_bytes(name_bytes)),
                                size,
                            );
                        }
                    }
                    VDIR => {
                        let Some((attr_ref_start, name_off, name_len)) = name_ref else {
                            continue;
                        };
                        let Some(name_bytes) =
                            read_attr_reference(buf, entry_end, attr_ref_start, name_off, name_len)
                        else {
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
                    _ => {} // VLNK, VSOCK, VBLK, VCHR, VFIFO: ignore
                }
            }
        }
    });

    partial
}

fn should_count_file(
    devid: Option<u64>,
    fileid: Option<u64>,
    linkcount: u32,
    context: &ScanContext,
) -> bool {
    if linkcount <= 1 {
        return true;
    }
    let Some(fileid) = fileid else {
        return true;
    };
    let identity = (devid.unwrap_or(context.policy.root_dev), fileid);
    context
        .seen_hard_links
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(identity)
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
    use std::fs;
    use std::os::unix::fs::symlink;

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
        assert_eq!(inaccessible_from_errno(Some(libc::ENOENT)), 0);
        assert_eq!(inaccessible_from_errno(None), 0);
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

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
