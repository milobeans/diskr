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
//!   [36..40] objtype         u32   — VREG=1, VDIR=2, VLNK=5, …
//!   [40..48] totalsize       u64   — present for regular files
//!   [48..56] allocsize       u64   — present for regular files
//!   [..    ] name bytes + padding

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::Scope;

// sys/attr.h constants (stable macOS ABI)
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x00000001;
const ATTR_CMN_OBJTYPE: u32 = 0x00000008;
const ATTR_CMN_ERROR: u32 = 0x20000000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x80000000;
const ATTR_FILE_TOTALSIZE: u32 = 0x00000002;
const ATTR_FILE_ALLOCSIZE: u32 = 0x00000004;
const FSOPT_PACK_INVAL_ATTRS: u64 = 0x00000008;
const ATTRIBUTE_SET_LEN: usize = 20;
const ATTR_REFERENCE_LEN: usize = 8;

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
/// Symlinks are skipped. Permission errors yield zero contribution, not panic.
pub fn scan_dir(root: &Path, top_file_limit: usize) -> DirScan {
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        return DirScan::default();
    };
    if !meta.file_type().is_dir() {
        return DirScan::default();
    }

    let aggregate = Mutex::new(ScanAggregate::default());

    rayon::scope(|scope| {
        spawn_scan(scope, root.to_path_buf(), top_file_limit, &aggregate);
    });

    aggregate
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .finish()
}

#[derive(Default)]
struct ScanAggregate {
    size: SizeInfo,
    largest_files: BinaryHeap<Reverse<FileCandidate>>,
}

impl ScanAggregate {
    fn merge(&mut self, partial: DirectoryScan, top_file_limit: usize) {
        self.size.add_file(partial.size);
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
        }
    }
}

#[derive(Default)]
struct DirectoryScan {
    size: SizeInfo,
    largest_files: BinaryHeap<Reverse<FileCandidate>>,
    subdirs: Vec<PathBuf>,
}

fn spawn_scan<'scope>(
    scope: &Scope<'scope>,
    dir: PathBuf,
    top_file_limit: usize,
    aggregate: &'scope Mutex<ScanAggregate>,
) {
    scope.spawn(move |scope| {
        let partial = scan_one_dir(&dir, top_file_limit);
        let DirectoryScan {
            size,
            largest_files,
            subdirs,
        } = partial;

        for subdir in subdirs {
            spawn_scan(scope, subdir, top_file_limit, aggregate);
        }
        let mut shared = aggregate.lock().unwrap();
        shared.merge(
            DirectoryScan {
                size,
                largest_files,
                subdirs: Vec::new(),
            },
            top_file_limit,
        );
    });
}

fn scan_one_dir(dir: &Path, top_file_limit: usize) -> DirectoryScan {
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
        return DirectoryScan::default();
    }

    let mut attrlist = Attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut partial = DirectoryScan::default();

    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                fd,
                &mut attrlist as *mut _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                FSOPT_PACK_INVAL_ATTRS,
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
            let Some(length) = read_u32(&buf, buf.len(), entry_start).map(|n| n as usize) else {
                break;
            };
            if length == 0 || entry_start + length > buf.len() {
                break;
            }
            let entry_end = entry_start + length;
            let Some(returned_common) = read_u32(&buf, entry_end, entry_start + 4) else {
                break;
            };
            let Some(returned_file) = read_u32(&buf, entry_end, entry_start + 16) else {
                break;
            };
            let mut field = entry_start + 4 + ATTRIBUTE_SET_LEN;

            let err = if returned_common & ATTR_CMN_ERROR != 0 {
                let Some(err) = read_u32(&buf, entry_end, field) else {
                    break;
                };
                field += 4;
                err
            } else {
                0
            };

            let name_bytes = if returned_common & ATTR_CMN_NAME != 0 {
                let attr_ref_start = field;
                let Some(name_off) = read_i32(&buf, entry_end, field).map(|n| n as isize) else {
                    break;
                };
                let Some(name_len) = read_u32(&buf, entry_end, field + 4).map(|n| n as usize)
                else {
                    break;
                };
                field += ATTR_REFERENCE_LEN;
                read_attr_reference(&buf, entry_end, attr_ref_start, name_off, name_len)
            } else {
                None
            };

            let objtype = if returned_common & ATTR_CMN_OBJTYPE != 0 {
                let Some(objtype) = read_u32(&buf, entry_end, field) else {
                    break;
                };
                field += 4;
                objtype
            } else {
                0
            };

            let totalsize = if returned_file & ATTR_FILE_TOTALSIZE != 0 {
                let value = read_u64(&buf, entry_end, field).unwrap_or(0);
                field += 8;
                value
            } else {
                0
            };
            let allocsize = if returned_file & ATTR_FILE_ALLOCSIZE != 0 {
                read_u64(&buf, entry_end, field).unwrap_or(0)
            } else {
                0
            };
            offset += length;

            if err != 0 {
                continue;
            }
            match objtype {
                VREG => {
                    let size = SizeInfo::new(totalsize, allocsize);
                    partial.size.add_file(size);
                    if let Some(name_bytes) = name_bytes {
                        push_largest_file(
                            &mut partial.largest_files,
                            top_file_limit,
                            dir.join(OsStr::from_bytes(name_bytes)),
                            size,
                        );
                    }
                }
                VDIR => {
                    let Some(name_bytes) = name_bytes else {
                        continue;
                    };
                    if matches!(name_bytes, b"." | b"..") {
                        continue;
                    }
                    partial
                        .subdirs
                        .push(dir.join(OsStr::from_bytes(name_bytes)));
                }
                _ => {} // VLNK, VSOCK, VBLK, VCHR, VFIFO: ignore
            }
        }
    }

    unsafe { libc::close(fd) };
    partial
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
        assert_eq!(scan.largest_files[0].path, root.join("a/large.bin"));
        assert_eq!(scan.largest_files[0].size.logical, 4096 * 8);
        assert_eq!(scan.largest_files[1].path, root.join("a/b/medium.bin"));
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

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
