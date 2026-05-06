use anyhow::{anyhow, Result};
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Delete by moving to the macOS Trash (reversible).
pub fn delete_to_trash(path: &Path) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("path contains a null byte"))?;

    unsafe {
        let _pool = AutoreleasePool::new()?;
        let path_string = msg_send_id_with_cstr(
            class(c"NSString")?,
            selector(c"stringWithUTF8String:"),
            path.as_ptr(),
        );
        if path_string.is_null() {
            return Err(anyhow!("could not create NSString for path"));
        }

        let url = msg_send_id_with_id(class(c"NSURL")?, selector(c"fileURLWithPath:"), path_string);
        if url.is_null() {
            return Err(anyhow!("could not create file URL for path"));
        }

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

unsafe fn msg_send_id_with_cstr(receiver: Id, sel: Sel, arg: *const libc::c_char) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, *const libc::c_char) -> Id =
        std::mem::transmute(objc_msgSend as *const ());
    f(receiver, sel, arg)
}

unsafe fn msg_send_id_with_id(receiver: Id, sel: Sel, arg: Id) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(objc_msgSend as *const ());
    f(receiver, sel, arg)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn rejects_paths_with_null_bytes() {
        let os_string = OsString::from_vec(b"/tmp/diskr\0bad".to_vec());
        let path = Path::new(&os_string);
        assert!(delete_to_trash(path).is_err());
    }
}
