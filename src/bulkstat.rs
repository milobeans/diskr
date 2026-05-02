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
//!   [..    ] name bytes + padding

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

// sys/attr.h constants (stable macOS ABI)
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x00000001;
const ATTR_CMN_OBJTYPE: u32 = 0x00000008;
const ATTR_CMN_ERROR: u32 = 0x20000000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x80000000;
const ATTR_FILE_TOTALSIZE: u32 = 0x00000002;
const FSOPT_PACK_INVAL_ATTRS: u64 = 0x00000008;
const ATTRIBUTE_SET_LEN: usize = 20;
const ATTR_REFERENCE_LEN: usize = 8;

// vnode types (sys/vnode.h)
const VREG: u32 = 1;
const VDIR: u32 = 2;

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

/// Recursive total size (logical bytes, matches Finder "Size") of `root`.
/// Symlinks are skipped. Permission errors yield zero contribution, not panic.
pub fn size_of_dir(root: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        return 0;
    };
    if !meta.file_type().is_dir() {
        return 0;
    }

    let mut attrlist = Attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_TOTALSIZE,
        forkattr: 0,
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut total: u64 = 0;

    while let Some(dir) = stack.pop() {
        let c_path = match CString::new(dir.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            continue;
        }
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
            if n < 0 {
                break;
            }
            if n == 0 {
                break;
            }
            let mut offset: usize = 0;
            for _ in 0..n {
                let entry_start = offset;
                if entry_start + 48 > buf.len() {
                    break;
                }
                let Some(length) = read_u32(&buf, buf.len(), entry_start).map(|n| n as usize)
                else {
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
                    let Some(name_off) = read_i32(&buf, entry_end, field).map(|n| n as isize)
                    else {
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
                        total = total.saturating_add(totalsize);
                    }
                    VDIR => {
                        let Some(name_bytes) = name_bytes else {
                            continue;
                        };
                        if matches!(name_bytes, b"." | b"..") {
                            continue;
                        }
                        stack.push(dir.join(OsStr::from_bytes(name_bytes)));
                    }
                    _ => {} // VLNK, VSOCK, VBLK, VCHR, VFIFO: ignore
                }
            }
        }
        unsafe { libc::close(fd) };
    }
    total
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

        let got = size_of_dir(&root);
        assert_eq!(got, 12 + 1_024_000 + 7, "bulkstat size mismatch");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_directory_counts_as_zero() {
        let root = test_root("missing");
        assert_eq!(size_of_dir(&root), 0);
    }

    #[test]
    fn root_symlink_to_directory_counts_as_zero() {
        let root = test_root("symlink_root");
        let target = root.join("target");
        let link = root.join("link");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("data.bin"), vec![1u8; 4096]).unwrap();
        symlink(&target, &link).unwrap();

        assert_eq!(size_of_dir(&link), 0);
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
