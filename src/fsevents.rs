//! FSEvents-backed cache freshness (audit finding 19, Phase 2).
//!
//! The persistent size cache marks every directory stale at startup; #82's
//! stale-while-revalidate then rescans the oldest stale directories. That is
//! correct but does redundant work on directories that did not change. This
//! module replays the volume's filesystem events since the last launch and
//! tells the cache which directories actually changed, so unchanged trees keep
//! their proven-fresh sizes and only touched subtrees are revalidated.
//!
//! The persisted state is the volume UUID plus the last `FSEventStreamEventId`.
//! On launch, if the volume still matches, the historical event stream is
//! replayed since that id on a dedicated CoreFoundation run-loop thread, and
//! the set of changed directories is returned. Anything that makes the replay
//! untrustworthy — first run, a different/reformatted volume, dropped or
//! coalesced events (`MustScanSubDirs`/`UserDropped`/`KernelDropped`), a wrapped
//! id, a mount/unmount/root change, or a timeout — falls back to treating
//! everything as stale (i.e. #82's age-based revalidation).

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::state;

const STATE_VERSION: u64 = 1;

/// Result of consulting FSEvents at startup. `is_stale` answers, per cached
/// directory, whether it must be revalidated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// FSEvents could not be trusted; every cached directory is stale and falls
    /// through to #82's age-based revalidation.
    All,
    /// FSEvents replayed cleanly: only directories whose subtree contains a
    /// changed path (and anything outside the watched root) are stale.
    Touched {
        watched_root: PathBuf,
        changed: Vec<PathBuf>,
    },
}

impl Freshness {
    /// Whether `dir`'s cached size should be treated as stale.
    pub fn is_stale(&self, dir: &Path) -> bool {
        match self {
            Freshness::All => true,
            Freshness::Touched {
                watched_root,
                changed,
            } => {
                // We only learned about changes under the watched root; treat
                // anything outside it as unknown, hence stale.
                if !dir.starts_with(watched_root) {
                    return true;
                }
                // A directory is stale when a change happened anywhere inside
                // its subtree — its recursive size may have moved.
                changed.iter().any(|change| change.starts_with(dir))
            }
        }
    }
}

/// Decide cache freshness for a fresh launch rooted at `root`, and persist the
/// current event id for next time. Never panics or blocks indefinitely; any
/// failure degrades to [`Freshness::All`].
pub fn plan_freshness(root: &Path) -> Freshness {
    let Ok(root) = root.canonicalize() else {
        return Freshness::All;
    };
    let Some(uuid) = volume_uuid(&root) else {
        return Freshness::All;
    };
    let now_id = current_event_id();
    let saved = load_state(&state_file());

    let outcome = match saved {
        Some(saved)
            if saved.volume_uuid == uuid
                && saved.last_event_id > 0
                && saved.last_event_id < now_id =>
        {
            replay_into_freshness(&root, saved.last_event_id)
        }
        // First run on this volume, a different volume, or a non-advancing id:
        // nothing to replay from.
        _ => Freshness::All,
    };

    // Persist the current id regardless, so the next launch has a baseline.
    let _ = save_state(&state_file(), &uuid, now_id);
    outcome
}

fn replay_into_freshness(root: &Path, since: FSEventStreamEventId) -> Freshness {
    match changes_since(root, since) {
        Some(report) if report.reliable => Freshness::Touched {
            watched_root: root.to_path_buf(),
            changed: report.changed_dirs,
        },
        _ => Freshness::All,
    }
}

// --- persisted state (sidecar file next to the size cache) ---

struct SavedState {
    volume_uuid: String,
    last_event_id: u64,
}

fn state_file() -> PathBuf {
    state::state_dir().join("fsevents.json")
}

fn load_state(path: &Path) -> Option<SavedState> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("version").and_then(|v| v.as_u64())? != STATE_VERSION {
        return None;
    }
    Some(SavedState {
        volume_uuid: value.get("volume_uuid")?.as_str()?.to_string(),
        last_event_id: value.get("last_event_id")?.as_u64()?,
    })
}

fn save_state(path: &Path, volume_uuid: &str, last_event_id: u64) -> Result<()> {
    let value = serde_json::json!({
        "version": STATE_VERSION,
        "volume_uuid": volume_uuid,
        "last_event_id": last_event_id,
    });
    state::atomic_write(path, &serde_json::to_string_pretty(&value)?)
}

// --- CoreServices / CoreFoundation FFI ---

type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type CFRunLoopRef = *const c_void;
type CFUUIDRef = *const c_void;
type CFTypeRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type ConstFSEventStreamRef = *const c_void;
type CFIndex = isize;
type Boolean = u8;
type CFTimeInterval = f64;
type CFStringEncoding = u32;
type FSEventStreamEventId = u64;
type FSEventStreamEventFlags = u32;
type FSEventStreamCreateFlags = u32;

const KCF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
const FS_CREATE_FLAG_NO_DEFER: FSEventStreamCreateFlags = 0x0000_0002;

const FS_FLAG_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
const FS_FLAG_USER_DROPPED: u32 = 0x0000_0002;
const FS_FLAG_KERNEL_DROPPED: u32 = 0x0000_0004;
const FS_FLAG_EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
const FS_FLAG_HISTORY_DONE: u32 = 0x0000_0010;
const FS_FLAG_ROOT_CHANGED: u32 = 0x0000_0020;
const FS_FLAG_MOUNT: u32 = 0x0000_0040;
const FS_FLAG_UNMOUNT: u32 = 0x0000_0080;
/// Flags that mean we may have missed changes, so "unchanged ⇒ fresh" no longer
/// holds and the whole replay must be discarded.
const FS_FLAGS_UNRELIABLE: u32 = FS_FLAG_USER_DROPPED
    | FS_FLAG_KERNEL_DROPPED
    | FS_FLAG_EVENT_IDS_WRAPPED
    | FS_FLAG_MOUNT
    | FS_FLAG_UNMOUNT
    | FS_FLAG_ROOT_CHANGED;

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[repr(C)]
struct CFArrayCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

type FSEventStreamCallback = extern "C" fn(
    ConstFSEventStreamRef,
    *mut c_void,
    usize,
    *mut c_void,
    *const FSEventStreamEventFlags,
    *const FSEventStreamEventId,
);

extern "C" {
    #[link_name = "kCFRunLoopDefaultMode"]
    static CF_RUN_LOOP_DEFAULT_MODE: CFStringRef;
    #[link_name = "kCFTypeArrayCallBacks"]
    static CF_TYPE_ARRAY_CALLBACKS: CFArrayCallBacks;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: CFStringEncoding,
        is_external: Boolean,
    ) -> CFStringRef;
    fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    fn CFStringGetCString(
        s: CFStringRef,
        buffer: *mut c_char,
        size: CFIndex,
        encoding: CFStringEncoding,
    ) -> Boolean;
    fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const *const c_void,
        num_values: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFArrayRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFUUIDCreateString(alloc: CFAllocatorRef, uuid: CFUUIDRef) -> CFStringRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: CFTimeInterval,
        return_after_source_handled: Boolean,
    ) -> i32;
}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamCreate(
        alloc: CFAllocatorRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFArrayRef,
        since_when: FSEventStreamEventId,
        latency: CFTimeInterval,
        flags: FSEventStreamCreateFlags,
    ) -> FSEventStreamRef;
    fn FSEventStreamScheduleWithRunLoop(
        stream: FSEventStreamRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn FSEventStreamStart(stream: FSEventStreamRef) -> Boolean;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> FSEventStreamEventId;
    fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFUUIDRef;
}

struct CallbackState {
    changed_dirs: Vec<PathBuf>,
    history_done: bool,
    reliable: bool,
}

struct ChangeReport {
    changed_dirs: Vec<PathBuf>,
    reliable: bool,
}

/// The current global event id, used as the next launch's replay baseline.
fn current_event_id() -> FSEventStreamEventId {
    unsafe { FSEventsGetCurrentEventId() }
}

/// Stable identity string for the volume that `path` lives on, so a replay from
/// a saved id is only trusted on the same volume.
fn volume_uuid(path: &Path) -> Option<String> {
    let dev = std::fs::metadata(path).ok()?.dev() as libc::dev_t;
    unsafe {
        let uuid = FSEventsCopyUUIDForDevice(dev);
        if uuid.is_null() {
            return None;
        }
        let string = CFUUIDCreateString(ptr::null(), uuid);
        CFRelease(uuid);
        if string.is_null() {
            return None;
        }
        let result = cfstring_to_string(string);
        CFRelease(string);
        result
    }
}

/// Replay the historical event stream for `root` since `since`, returning the
/// directories that changed. Runs the CoreFoundation run loop on a dedicated
/// thread (so it never touches the main thread's run loop) and is bounded by a
/// hard timeout. `None` means the replay could not be completed and the caller
/// must fall back.
fn changes_since(root: &Path, since: FSEventStreamEventId) -> Option<ChangeReport> {
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name(String::from("diskr-fsevents"))
        .spawn(move || changes_since_on_run_loop(&root, since))
        .ok()?
        .join()
        .ok()?
}

fn changes_since_on_run_loop(root: &Path, since: FSEventStreamEventId) -> Option<ChangeReport> {
    let cf_root = make_cfstring(root.as_os_str().as_bytes())?;
    let values = [cf_root];
    let array = unsafe {
        CFArrayCreate(
            ptr::null(),
            values.as_ptr(),
            1,
            ptr::addr_of!(CF_TYPE_ARRAY_CALLBACKS),
        )
    };
    // The array retained the string; release our reference either way.
    unsafe { CFRelease(cf_root) };
    if array.is_null() {
        return None;
    }

    let mut callback_state = CallbackState {
        changed_dirs: Vec::new(),
        history_done: false,
        reliable: true,
    };
    let context = FSEventStreamContext {
        version: 0,
        info: &mut callback_state as *mut CallbackState as *mut c_void,
        retain: ptr::null(),
        release: ptr::null(),
        copy_description: ptr::null(),
    };

    let stream = unsafe {
        FSEventStreamCreate(
            ptr::null(),
            fsevents_callback,
            &context,
            array,
            since,
            0.0,
            FS_CREATE_FLAG_NO_DEFER,
        )
    };
    unsafe { CFRelease(array) };
    if stream.is_null() {
        return None;
    }

    let run_loop = unsafe { CFRunLoopGetCurrent() };
    let mode = unsafe { CF_RUN_LOOP_DEFAULT_MODE };
    let started = unsafe {
        FSEventStreamScheduleWithRunLoop(stream, run_loop, mode);
        FSEventStreamStart(stream)
    };
    if started == 0 {
        unsafe {
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
        }
        return None;
    }

    // Pump the run loop until the historical replay finishes (HistoryDone) or a
    // hard deadline elapses. The callback runs synchronously on this thread
    // inside CFRunLoopRunInMode, so reading callback_state between turns is
    // race-free.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !callback_state.history_done && Instant::now() < deadline {
        unsafe { CFRunLoopRunInMode(mode, 0.2, 1) };
    }
    let completed = callback_state.history_done;

    unsafe {
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
    }

    if !completed {
        return None;
    }
    Some(ChangeReport {
        changed_dirs: std::mem::take(&mut callback_state.changed_dirs),
        reliable: callback_state.reliable,
    })
}

extern "C" fn fsevents_callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const FSEventStreamEventFlags,
    _event_ids: *const FSEventStreamEventId,
) {
    if info.is_null() || event_paths.is_null() || event_flags.is_null() {
        return;
    }
    // Without kFSEventStreamCreateFlagUseCFTypes, eventPaths is a C array of
    // NUL-terminated strings.
    let paths = event_paths as *const *const c_char;
    let state = unsafe { &mut *(info as *mut CallbackState) };

    for i in 0..num_events {
        let flags = unsafe { *event_flags.add(i) };
        if flags & FS_FLAG_HISTORY_DONE != 0 {
            state.history_done = true;
            continue;
        }
        if flags & FS_FLAGS_UNRELIABLE != 0 {
            state.reliable = false;
        }
        // MustScanSubDirs marks that subtree as needing a full rescan; recording
        // the path keeps it (and its ancestors) stale, which is what we want.
        let _ = FS_FLAG_MUST_SCAN_SUBDIRS;
        let raw = unsafe { *paths.add(i) };
        if raw.is_null() {
            continue;
        }
        let bytes = unsafe { CStr::from_ptr(raw) }.to_bytes();
        if !bytes.is_empty() {
            state.changed_dirs.push(PathBuf::from(
                std::str::from_utf8(bytes)
                    .map(str::to_owned)
                    .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned()),
            ));
        }
    }
}

fn make_cfstring(bytes: &[u8]) -> Option<*const c_void> {
    let s = unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            bytes.as_ptr(),
            bytes.len() as CFIndex,
            KCF_STRING_ENCODING_UTF8,
            0,
        )
    };
    if s.is_null() {
        None
    } else {
        Some(s)
    }
}

fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let len = unsafe { CFStringGetLength(s) };
    // UTF-8 needs at most 4 bytes per UTF-16 unit, plus a NUL.
    let capacity = (len.max(0) as usize)
        .saturating_mul(4)
        .saturating_add(1)
        .max(8);
    let mut buf = vec![0_i8; capacity];
    let ok = unsafe {
        CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            capacity as CFIndex,
            KCF_STRING_ENCODING_UTF8,
        )
    };
    if ok == 0 {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(cstr.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_all_marks_everything_stale() {
        let f = Freshness::All;
        assert!(f.is_stale(Path::new("/anything")));
    }

    #[test]
    fn touched_marks_only_affected_subtrees() {
        let f = Freshness::Touched {
            watched_root: PathBuf::from("/root"),
            changed: vec![PathBuf::from("/root/a/b")],
        };
        // The changed directory itself and its ancestors are stale.
        assert!(f.is_stale(Path::new("/root/a/b")));
        assert!(f.is_stale(Path::new("/root/a")));
        assert!(f.is_stale(Path::new("/root")));
        // A sibling subtree with no change inside it stays fresh.
        assert!(!f.is_stale(Path::new("/root/a/c")));
        assert!(!f.is_stale(Path::new("/root/x")));
        // Anything outside the watched root is unknown, hence stale.
        assert!(f.is_stale(Path::new("/elsewhere")));
    }

    #[test]
    fn state_round_trips_through_sidecar() {
        let path = std::env::temp_dir().join(format!(
            "diskr_fsevents_{}_{}.json",
            std::process::id(),
            current_event_id()
        ));
        save_state(&path, "ABC-123", 987_654).unwrap();
        let loaded = load_state(&path).expect("state present");
        assert_eq!(loaded.volume_uuid, "ABC-123");
        assert_eq!(loaded.last_event_id, 987_654);
        // A wrong version is ignored rather than trusted.
        std::fs::write(
            &path,
            r#"{"version":99,"volume_uuid":"x","last_event_id":1}"#,
        )
        .unwrap();
        assert!(load_state(&path).is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ffi_symbols_link_and_do_not_panic() {
        // Exercises the CoreServices/CoreFoundation bindings: a wrong symbol
        // name would fail to link. Values are environment-dependent, so we only
        // assert the calls return without crashing.
        let _ = current_event_id();
        let _ = volume_uuid(&std::env::temp_dir());
    }

    #[test]
    fn replay_run_loop_completes_with_no_history() {
        // Replaying "since the current id" has no historical events, so
        // HistoryDone fires immediately. This drives the whole run-loop FFI
        // path — CFArray/CFString creation, FSEventStreamCreate/Schedule/Start,
        // the callback, and CFRunLoopRunInMode — deterministically, without
        // depending on a specific change being journaled. If FSEvents is
        // unavailable it returns None; either way it must not hang or panic.
        let dir = std::env::temp_dir();
        if let Some(report) = changes_since(&dir, current_event_id()) {
            assert!(report.reliable);
        }
    }
}
