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
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use rayon::Scope;

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
const VOLUMES_ROOT: &str = "/Volumes";

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
    /// Directories that could not be opened due to permissions (EACCES/EPERM).
    /// When non-zero the reported size is a lower bound.
    pub inaccessible: u32,
    /// Mounted volumes skipped below /Volumes to avoid crossing filesystem boundaries.
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
/// Symlinks are skipped. Permission errors yield zero contribution and are counted.
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
    let context = ScanContext {
        policy: ScanPolicy::new(&policy_root, meta.dev()),
        seen_hard_links: Mutex::new(HashSet::new()),
    };
    let aggregate = Mutex::new(ScanAggregate::default());

    rayon::scope(|scope| {
        spawn_scan(
            scope,
            root.to_path_buf(),
            top_file_limit,
            &aggregate,
            &context,
            cancellation,
        );
    });

    if cancellation.is_cancelled() {
        return None;
    }

    Some(
        aggregate
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .finish(),
    )
}

#[derive(Default)]
struct ScanAggregate {
    size: SizeInfo,
    largest_files: BinaryHeap<Reverse<FileCandidate>>,
    inaccessible: u32,
    skipped_mounts: u32,
}

impl ScanAggregate {
    fn merge(&mut self, partial: DirectoryScan, top_file_limit: usize) {
        self.size.add_file(partial.size);
        self.inaccessible = self.inaccessible.saturating_add(partial.inaccessible);
        self.skipped_mounts = self.skipped_mounts.saturating_add(partial.skipped_mounts);
        if top_file_limit == 0 {
            return;
        }
        for Reverse(candidate) in partial.largest_files {
            push_file_candidate(&mut self.largest_files, top_file_limit, candidate);
        }
    }

    fn finish(self) -> DirScan {
        DirScan {
            size: self.size,
            largest_files: sorted_largest_files(self.largest_files),
            inaccessible: self.inaccessible,
            skipped_mounts: self.skipped_mounts,
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
    skip_data_volume: bool,
}

impl ScanPolicy {
    fn new(root: &Path, root_dev: u64) -> Self {
        let data_root = Path::new(DATA_VOLUME_ROOT);
        Self {
            root_dev,
            skip_data_volume: data_root.starts_with(root) && root != data_root,
        }
    }
}

fn spawn_scan<'scope>(
    scope: &Scope<'scope>,
    dir: PathBuf,
    top_file_limit: usize,
    aggregate: &'scope Mutex<ScanAggregate>,
    context: &'scope ScanContext,
    cancellation: &'scope ScanCancellation,
) {
    scope.spawn(move |scope| {
        if cancellation.is_cancelled() {
            return;
        }
        let partial = scan_one_dir(&dir, top_file_limit, context, cancellation);
        if cancellation.is_cancelled() {
            return;
        }
        let DirectoryScan {
            size,
            largest_files,
            subdirs,
            inaccessible,
            skipped_mounts,
        } = partial;

        for subdir in subdirs {
            if cancellation.is_cancelled() {
                return;
            }
            spawn_scan(
                scope,
                subdir,
                top_file_limit,
                aggregate,
                context,
                cancellation,
            );
        }
        if cancellation.is_cancelled() {
            return;
        }
        let mut shared = aggregate.lock().unwrap();
        shared.merge(
            DirectoryScan {
                size,
                largest_files,
                subdirs: Vec::new(),
                inaccessible,
                skipped_mounts,
            },
            top_file_limit,
        );
    });
}

fn scan_one_dir(
    dir: &Path,
    top_file_limit: usize,
    context: &ScanContext,
    cancellation: &ScanCancellation,
) -> DirectoryScan {
    if cancellation.is_cancelled() {
        return DirectoryScan::default();
    }
    let c_path = match CString::new(dir.as_os_str().as_bytes()) {
        Ok(p) => p,
        Err(_) => return DirectoryScan::default(),
    };
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        // EACCES/EPERM means real data was skipped (no Full Disk Access, ACLs);
        // ENOENT is just a deletion race during the walk and stays silent.
        let errno = std::io::Error::last_os_error().raw_os_error();
        let inaccessible = u32::from(matches!(errno, Some(libc::EACCES) | Some(libc::EPERM)));
        return DirectoryScan {
            inaccessible,
            ..DirectoryScan::default()
        };
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

                if err != 0 {
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

    unsafe { libc::close(fd) };
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

fn should_skip_subdir(path: &Path, devid: Option<u64>, policy: &ScanPolicy) -> bool {
    if policy.skip_data_volume && path == Path::new(DATA_VOLUME_ROOT) {
        return true;
    }
    path.starts_with(VOLUMES_ROOT) && devid.is_some_and(|dev| dev != policy.root_dev)
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
    fn skip_policy_skips_data_volume_only_from_ancestor() {
        let root_policy = ScanPolicy::new(Path::new("/"), 10);
        assert!(should_skip_subdir(
            Path::new(DATA_VOLUME_ROOT),
            Some(10),
            &root_policy
        ));

        let data_policy = ScanPolicy::new(Path::new(DATA_VOLUME_ROOT), 10);
        assert!(!should_skip_subdir(
            Path::new(DATA_VOLUME_ROOT),
            Some(10),
            &data_policy
        ));
    }

    #[test]
    fn skip_policy_skips_different_devices_under_volumes() {
        let policy = ScanPolicy::new(Path::new("/"), 10);

        assert!(should_skip_subdir(
            Path::new("/Volumes/External"),
            Some(20),
            &policy
        ));
        assert!(!should_skip_subdir(
            Path::new("/Volumes/LocalDirectory"),
            Some(10),
            &policy
        ));
        assert!(!should_skip_subdir(
            Path::new("/Users/example"),
            Some(20),
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
