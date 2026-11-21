use anyhow::{anyhow, Context, Result};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::bulkstat::FileIdentity;
use std::process::Command;
use std::time::Duration;

use crate::process;

const OSASCRIPT_PATH: &str = "/usr/bin/osascript";

/// Rename atomically only if no directory entry already occupies the destination.
pub fn rename_without_replacement(from: &Path, to: &Path) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a null byte"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a null byte"))?;
    // The kernel checks the destination and renames under one exclusive operation.
    // Unsupported filesystems must fail rather than fall back to an overwriting rename.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The decision shown in the confirmation dialog, including the directory entry
/// that was selected when the scan produced the row.
pub struct TrashTarget {
    pub path: PathBuf,
    parent: FileIdentity,
    item: FileIdentity,
    link_id: u64,
}

impl TrashTarget {
    pub fn capture(path: &Path, expected: Option<FileIdentity>) -> Result<Self> {
        let (parent, item) = open_trash_entry(path)?;
        let item_id = crate::bulkstat::metadata_identity(&item.metadata()?);
        anyhow::ensure!(
            expected == Some(item_id),
            "item changed since scan; rescan before moving it to Trash"
        );
        Ok(Self {
            path: path.to_owned(),
            parent: crate::bulkstat::metadata_identity(&parent.metadata()?),
            item: item_id,
            link_id: link_id(&item)
                .context("filesystem cannot identify the selected directory entry")?,
        })
    }

    pub fn move_with(&self, move_item: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        let (parent, item) = open_trash_entry(&self.path)?;
        anyhow::ensure!(
            crate::bulkstat::metadata_identity(&parent.metadata()?) == self.parent
                && crate::bulkstat::metadata_identity(&item.metadata()?) == self.item
                && link_id(&item)? == self.link_id,
            "item or parent changed since confirmation; rescan before moving it to Trash"
        );
        // Darwin volfs addresses this exact directory entry by persistent link ID,
        // including for hard links and symlink leaves. Keep the opened objects alive
        // throughout the call so their IDs cannot be recycled under the worker.
        let reference = PathBuf::from(format!("/.vol/{}/{}", self.item.0, self.link_id));
        let name = CString::new(reference.as_os_str().as_bytes())?;
        let resolved = open_event_fd(libc::AT_FDCWD, &name, libc::O_SYMLINK)?;
        anyhow::ensure!(
            crate::bulkstat::metadata_identity(&resolved.metadata()?) == self.item
                && link_id(&resolved)? == self.link_id,
            "cannot resolve the selected item by identity"
        );
        move_item(&reference)
    }
}

fn open_trash_entry(path: &Path) -> Result<(File, File)> {
    anyhow::ensure!(
        path.is_absolute() && !path.components().any(|c| c == Component::ParentDir),
        "expected an absolute path without parent traversal"
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("cannot move a filesystem root to Trash"))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("missing item name"))?;
    // These root-owned macOS aliases also occur in the system temporary directory.
    let parent = if ["/tmp", "/var", "/etc"]
        .iter()
        .any(|alias| parent.starts_with(alias))
    {
        Path::new("/private").join(parent.strip_prefix("/")?)
    } else {
        parent.to_owned()
    };
    let parent = CString::new(parent.as_os_str().as_bytes())?;
    let name = CString::new(name.as_bytes())?;
    let dir = open_event_fd(
        libc::AT_FDCWD,
        &parent,
        libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY,
    )?;
    let item = open_event_fd(dir.as_raw_fd(), &name, libc::O_SYMLINK)?;
    Ok((dir, item))
}

fn open_event_fd(dir: libc::c_int, path: &CStr, flags: libc::c_int) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            dir,
            path.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_EVTONLY,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn link_id(file: &File) -> io::Result<u64> {
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: libc::ATTR_CMNEXT_LINKID,
    };
    // getattrlist packs the eight-byte link ID directly after a four-byte length.
    let mut buf = [0u8; 12];
    if unsafe {
        libc::fgetattrlist(
            file.as_raw_fd(),
            (&raw mut attrs).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if u32::from_ne_bytes(buf[..4].try_into().unwrap()) != buf.len() as u32 {
        return Err(io::Error::other(
            "filesystem did not return a link identifier",
        ));
    }
    Ok(u64::from_ne_bytes(buf[4..].try_into().unwrap()))
}

/// Resolve the identity-based volfs path to a Foundation file reference for Trash.
pub fn delete_to_trash(path: &Path) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;

    unsafe {
        let _pool = AutoreleasePool::new()?;
        let url = file_reference_url(path.as_ptr())?;

        let manager = msg_send_id(class(c"NSFileManager")?, selector(c"defaultManager"));
        if manager.is_null() {
            return Err(anyhow!("could not access NSFileManager"));
        }

        let mut resulting_url: Id = std::ptr::null_mut();
        let mut error: Id = std::ptr::null_mut();
        let ok = msg_send_bool_with_trash_args(
            manager,
            selector(c"trashItemAtURL:resultingItemURL:error:"),
            url,
            &mut resulting_url,
            &mut error,
        );

        if ok {
            Ok(())
        } else {
            Err(anyhow!("move to Trash failed: {}", error_message(error)))
        }
    }
}

type Id = *mut libc::c_void;
type Sel = *mut libc::c_void;

#[link(name = "Foundation", kind = "framework")]
extern "C" {}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const libc::c_char) -> Id;
    fn sel_registerName(name: *const libc::c_char) -> Sel;
    fn objc_msgSend();
}

struct AutoreleasePool {
    id: Id,
}

impl AutoreleasePool {
    unsafe fn new() -> Result<Self> {
        let allocated = msg_send_id(class(c"NSAutoreleasePool")?, selector(c"alloc"));
        let id = msg_send_id(allocated, selector(c"init"));
        if id.is_null() {
            Err(anyhow!("could not create autorelease pool"))
        } else {
            Ok(Self { id })
        }
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe {
            msg_send_void(self.id, selector(c"drain"));
        }
    }
}

fn class(name: &CStr) -> Result<Id> {
    let class = unsafe { objc_getClass(name.as_ptr()) };
    if class.is_null() {
        Err(anyhow!(
            "Objective-C class not found: {}",
            name.to_string_lossy()
        ))
    } else {
        Ok(class)
    }
}

fn selector(name: &CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}

unsafe fn msg_send_id(receiver: Id, sel: Sel) -> Id {
    let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as *const ());
    f(receiver, sel)
}

unsafe fn file_url(path: *const libc::c_char) -> Result<Id> {
    let f: unsafe extern "C" fn(Id, Sel, *const libc::c_char, libc::c_schar, Id) -> Id =
        std::mem::transmute(objc_msgSend as *const ());
    Ok(f(
        class(c"NSURL")?,
        selector(c"fileURLWithFileSystemRepresentation:isDirectory:relativeToURL:"),
        path,
        0,
        std::ptr::null_mut(),
    ))
}

unsafe fn file_reference_url(path: *const libc::c_char) -> Result<Id> {
    let url = file_url(path)?;
    let reference = msg_send_id(url, selector(c"fileReferenceURL"));
    anyhow::ensure!(
        !reference.is_null(),
        "filesystem does not support a stable Trash reference"
    );
    Ok(reference)
}

unsafe fn msg_send_bool_with_trash_args(
    receiver: Id,
    sel: Sel,
    url: Id,
    resulting_url: *mut Id,
    error: *mut Id,
) -> bool {
    let f: unsafe extern "C" fn(Id, Sel, Id, *mut Id, *mut Id) -> libc::c_schar =
        std::mem::transmute(objc_msgSend as *const ());
    f(receiver, sel, url, resulting_url, error) != 0
}

unsafe fn msg_send_const_char(receiver: Id, sel: Sel) -> *const libc::c_char {
    let f: unsafe extern "C" fn(Id, Sel) -> *const libc::c_char =
        std::mem::transmute(objc_msgSend as *const ());
    f(receiver, sel)
}

unsafe fn msg_send_void(receiver: Id, sel: Sel) {
    let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc_msgSend as *const ());
    f(receiver, sel);
}

unsafe fn error_message(error: Id) -> String {
    if error.is_null() {
        return String::from("unknown error");
    }
    let description = msg_send_id(error, selector(c"localizedDescription"));
    if description.is_null() {
        return String::from("unknown error");
    }
    let bytes = msg_send_const_char(description, selector(c"UTF8String"));
    if bytes.is_null() {
        String::from("unknown error")
    } else {
        CStr::from_ptr(bytes).to_string_lossy().into_owned()
    }
}

/// Empty the macOS Trash permanently using osascript.
///
/// This action is irreversible: files deleted from the Trash cannot be recovered.
pub fn empty_trash() -> Result<()> {
    empty_trash_with(run_osascript)
}

/// Empty the macOS Trash using the provided command runner.
///
/// The runner receives the absolute program path and its arguments. Production code
/// passes [`run_osascript`]; tests pass a fake that records invocations without
/// touching the real Trash.
pub fn empty_trash_with(runner: fn(&str, &[&str]) -> Result<()>) -> Result<()> {
    runner(
        OSASCRIPT_PATH,
        &["-e", "tell application \"Finder\" to empty trash"],
    )
}

/// Run an external command, returning an error if it exits non-zero.
pub fn run_osascript(program: &str, args: &[&str]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    let output = process::run_process(command, Duration::from_secs(300));

    if output.timed_out {
        return Err(anyhow!(
            "{program} timed out; check the Trash before retrying"
        ));
    }
    if !output.success {
        return Err(anyhow!("{program} failed: {}", output.stderr.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct RenameFixture(PathBuf);

    impl RenameFixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "diskr_rename_{}_{}_{}",
                std::process::id(),
                timestamp,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for RenameFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exclusive_rename_preserves_files_directories_and_symlinks() {
        let root = RenameFixture::new();
        let source = root.0.join(" original-é.txt ");
        let destination = root.0.join(" renamed-界.txt ");
        fs::write(&source, b"file contents").unwrap();
        rename_without_replacement(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"file contents");

        let source_dir = root.0.join("source-dir");
        let destination_dir = root.0.join("destination-dir");
        fs::create_dir(&source_dir).unwrap();
        fs::write(source_dir.join("child"), b"directory contents").unwrap();
        rename_without_replacement(&source_dir, &destination_dir).unwrap();
        assert!(!source_dir.exists());
        assert_eq!(
            fs::read(destination_dir.join("child")).unwrap(),
            b"directory contents"
        );

        let source_link = root.0.join("source-link");
        let destination_link = root.0.join("destination-link");
        std::os::unix::fs::symlink("missing-target", &source_link).unwrap();
        rename_without_replacement(&source_link, &destination_link).unwrap();
        assert!(fs::symlink_metadata(&source_link).is_err());
        assert_eq!(
            fs::read_link(destination_link).unwrap(),
            Path::new("missing-target")
        );
    }

    #[test]
    fn exclusive_rename_refuses_existing_files_and_directories() {
        let root = RenameFixture::new();
        let source = root.0.join("source");
        let destination = root.0.join("destination");
        fs::write(&source, b"source contents").unwrap();
        fs::write(&destination, b"destination contents").unwrap();
        assert_eq!(
            rename_without_replacement(&source, &destination)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&source).unwrap(), b"source contents");
        assert_eq!(fs::read(&destination).unwrap(), b"destination contents");

        let source_dir = root.0.join("source-dir");
        let destination_dir = root.0.join("destination-dir");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination_dir).unwrap();
        assert_eq!(
            rename_without_replacement(&source_dir, &destination_dir)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(source_dir.is_dir());
        assert!(destination_dir.is_dir());
    }

    #[test]
    fn exclusive_rename_refuses_dangling_and_live_destination_symlinks() {
        let root = RenameFixture::new();
        let source = root.0.join("source");
        let destination = root.0.join("destination");
        let target = root.0.join("target");
        fs::write(&source, b"source contents").unwrap();
        std::os::unix::fs::symlink("target", &destination).unwrap();
        for target_exists in [false, true] {
            if target_exists {
                fs::write(&target, b"target contents").unwrap();
            }
            assert_eq!(
                rename_without_replacement(&source, &destination)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(fs::read(&source).unwrap(), b"source contents");
            assert_eq!(fs::read_link(&destination).unwrap(), Path::new("target"));
            if target_exists {
                assert_eq!(fs::read(&target).unwrap(), b"target contents");
            }
        }
    }

    #[test]
    fn exclusive_rename_keeps_the_losing_source_when_destinations_race() {
        let root = RenameFixture::new();
        let sources = [root.0.join("first"), root.0.join("second")];
        fs::write(&sources[0], b"first contents").unwrap();
        fs::write(&sources[1], b"second contents").unwrap();
        let destination = root.0.join("destination");
        let start = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let start = &start;
            let destination = &destination;
            let workers = sources.each_ref().map(|source| {
                scope.spawn(move || {
                    start.wait();
                    rename_without_replacement(source, destination)
                })
            });
            workers.map(|worker| worker.join().unwrap())
        });
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = outcomes.iter().position(|result| result.is_ok()).unwrap();
        let loser = 1 - winner;
        assert_eq!(
            outcomes[loser].as_ref().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!sources[winner].exists());
        let expected = [b"first contents".as_slice(), b"second contents".as_slice()];
        assert_eq!(fs::read(&destination).unwrap(), expected[winner]);
        assert_eq!(fs::read(&sources[loser]).unwrap(), expected[loser]);
    }

    #[test]
    fn exclusive_rename_rejects_null_bytes_without_changing_the_source() {
        let root = RenameFixture::new();
        let source = root.0.join("source");
        fs::write(&source, b"source contents").unwrap();
        let invalid = OsString::from_vec(b"invalid\0name".to_vec());
        for (from, to) in [
            (source.as_path(), Path::new(&invalid)),
            (Path::new(&invalid), source.as_path()),
        ] {
            assert_eq!(
                rename_without_replacement(from, to).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(fs::read(source).unwrap(), b"source contents");
    }

    #[test]
    fn filesystem_url_preserves_path_without_trashing_anything() {
        let path = c"/tmp/diskr URL with spaces and unicode-é";
        unsafe {
            let _pool = AutoreleasePool::new().unwrap();
            let url = file_url(path.as_ptr()).unwrap();
            assert!(!url.is_null());
            let actual = msg_send_const_char(url, selector(c"fileSystemRepresentation"));
            assert!(!actual.is_null());
            assert_eq!(CStr::from_ptr(actual), path);
        }
    }
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn trash_reference_preserves_each_directory_entry() {
        use std::os::unix::fs::symlink;
        let root = RenameFixture::new();
        let directory = root.0.canonicalize().unwrap();
        fs::write(directory.join("original"), "data").unwrap();
        fs::hard_link(directory.join("original"), directory.join("second")).unwrap();
        symlink("original", directory.join("symlink")).unwrap();
        symlink("missing", directory.join("dangling")).unwrap();
        fs::create_dir(directory.join("subdir")).unwrap();
        for name in ["original", "second", "symlink", "dangling", "subdir"] {
            let path = directory.join(name);
            let target =
                TrashTarget::capture(&path, crate::bulkstat::path_identity(&path)).unwrap();
            target
                .move_with(|reference| {
                    let reference = CString::new(reference.as_os_str().as_bytes())?;
                    unsafe {
                        let _pool = AutoreleasePool::new()?;
                        let url = file_reference_url(reference.as_ptr())?;
                        let resolved = msg_send_id(url, selector(c"filePathURL"));
                        let bytes =
                            msg_send_const_char(resolved, selector(c"fileSystemRepresentation"));
                        assert!(!bytes.is_null());
                        assert_eq!(
                            CStr::from_ptr(bytes).to_bytes(),
                            path.as_os_str().as_bytes()
                        );
                    }
                    Ok(())
                })
                .unwrap();
        }
    }

    #[test]
    fn trash_requires_the_scanned_leaf_and_confirmed_parent() {
        let root = RenameFixture::new();
        let parent = root.0.join("parent");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("file");
        fs::write(&path, "data").unwrap();
        let scanned = crate::bulkstat::path_identity(&path);
        let target = TrashTarget::capture(&path, scanned).unwrap();
        fs::rename(&parent, root.0.join("before")).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::hard_link(root.0.join("before/file"), &path).unwrap();
        assert!(target.move_with(|_| panic!("changed parent")).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement").unwrap();
        assert!(TrashTarget::capture(&path, scanned).is_err());
        fs::remove_dir_all(&parent).unwrap();
        std::os::unix::fs::symlink(root.0.join("before"), &parent).unwrap();
        assert!(TrashTarget::capture(&path, scanned).is_err());
        assert!(target.move_with(|_| panic!("symlink ancestor")).is_err());
    }

    #[test]
    fn rejects_paths_with_null_bytes() {
        let os_string = OsString::from_vec(b"/tmp/diskr\0bad".to_vec());
        let path = Path::new(&os_string);
        assert!(delete_to_trash(path).is_err());
    }

    // Verify that empty_trash_with passes the correct osascript invocation to the runner
    // and that the runner result is propagated correctly — without calling the real Finder.
    #[test]
    fn empty_trash_invokes_runner_with_osascript_args() {
        // fn pointers cannot close over state, so use a thread-local to capture calls.
        use std::cell::RefCell;
        thread_local! {
            static CAPTURED: RefCell<Vec<(String, Vec<String>)>> = const { RefCell::new(Vec::new()) };
        }

        fn fake_runner(program: &str, args: &[&str]) -> Result<()> {
            CAPTURED.with(|c| {
                c.borrow_mut().push((
                    program.to_string(),
                    args.iter().map(|s| s.to_string()).collect(),
                ));
            });
            Ok(())
        }

        let result = empty_trash_with(fake_runner);
        assert!(result.is_ok());

        CAPTURED.with(|c| {
            let captured = c.borrow();
            assert_eq!(captured.len(), 1);
            let (prog, args) = &captured[0];
            assert_eq!(prog, OSASCRIPT_PATH);
            assert_eq!(args, &["-e", "tell application \"Finder\" to empty trash"]);
        });
    }

    #[test]
    fn empty_trash_propagates_runner_error() {
        fn failing_runner(_program: &str, _args: &[&str]) -> Result<()> {
            Err(anyhow!("simulated failure"))
        }

        let result = empty_trash_with(failing_runner);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated failure"));
    }
}
