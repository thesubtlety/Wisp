//! macOS probes for meeting detection: which processes run microphone input (Core Audio process
//! objects, macOS 14.2+), and the titles of each app's windows (CoreGraphics window list).
//!
//! Raw FFI kept small: every function returns plain Rust data, and all ownership (CFRelease,
//! autorelease pools) is handled here.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};

use wisp_core::meeting::{MicProcess, WindowInfo};

type OSStatus = i32;
type AudioObjectID = u32;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNumberRef = *const c_void;
type CFIndex = isize;

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const SYSTEM_OBJECT: AudioObjectID = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const ELEMENT_MAIN: u32 = 0;
const PROCESS_OBJECT_LIST: u32 = fourcc(b"prs#");
const PROCESS_IS_RUNNING_INPUT: u32 = fourcc(b"piri");
const PROCESS_BUNDLE_ID: u32 = fourcc(b"pbid");
const PROCESS_PID: u32 = fourcc(b"ppid");

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT32: CFIndex = 3;
const WINDOW_LIST_ALL: u32 = 0;
const WINDOW_LIST_EXCLUDE_DESKTOP: u32 = 1 << 4;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: CFTypeRef);
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetLength(string: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        size: CFIndex,
        encoding: u32,
    ) -> bool;
    fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: CFNumberRef, kind: CFIndex, out: *mut c_void) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    static kCGWindowOwnerPID: CFStringRef;
    static kCGWindowName: CFStringRef;
    static kCGWindowLayer: CFStringRef;
}

/// Core Audio can't list processes (macOS before 14.2, or the call failed).
#[derive(Debug)]
pub(crate) struct Unsupported;

/// Every Core Audio process object, with whether it is running microphone input right now.
pub(crate) fn audio_processes() -> Result<Vec<(MicProcess, bool)>, Unsupported> {
    let ids = read_process_ids()?;
    Ok(ids
        .into_iter()
        .filter_map(|id| {
            let pid = read_u32(id, PROCESS_PID)? as i32;
            let running = read_u32(id, PROCESS_IS_RUNNING_INPUT).unwrap_or(0) != 0;
            let bundle_id = read_cfstring_property(id, PROCESS_BUNDLE_ID).unwrap_or_default();
            Some((MicProcess { pid, bundle_id }, running))
        })
        .collect())
}

/// The processes running microphone input, excluding `own_pid`.
pub(crate) fn mic_processes(own_pid: i32) -> Result<Vec<MicProcess>, Unsupported> {
    Ok(audio_processes()?
        .into_iter()
        .filter(|(p, running)| *running && p.pid != own_pid)
        .map(|(p, _)| p)
        .collect())
}

fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        selector,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    }
}

fn read_process_ids() -> Result<Vec<AudioObjectID>, Unsupported> {
    let addr = address(PROCESS_OBJECT_LIST);
    let mut size = 0u32;
    // SAFETY: valid address struct and out-pointer; no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size)
    };
    if status != 0 {
        return Err(Unsupported);
    }
    let count = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut ids = vec![0 as AudioObjectID; count];
    if count == 0 {
        return Ok(ids);
    }
    // SAFETY: `ids` holds `size` bytes; Core Audio writes at most that many and updates `size`.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return Err(Unsupported);
    }
    ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
    Ok(ids)
}

fn read_u32(object: AudioObjectID, selector: u32) -> Option<u32> {
    let addr = address(selector);
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `value` is a u32 and `size` says so.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut u32).cast(),
        )
    };
    (status == 0).then_some(value)
}

fn read_cfstring_property(object: AudioObjectID, selector: u32) -> Option<String> {
    let addr = address(selector);
    let mut value: CFStringRef = std::ptr::null();
    let mut size = std::mem::size_of::<CFStringRef>() as u32;
    // SAFETY: `value` is a pointer-sized slot for the returned (+1 retained) CFString.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut CFStringRef).cast(),
        )
    };
    if status != 0 || value.is_null() {
        return None;
    }
    let text = cfstring_to_string(value);
    // SAFETY: the property getter hands us ownership of the string.
    unsafe { CFRelease(value) };
    text
}

/// Copies a CFString into a Rust `String`. Does not release it.
fn cfstring_to_string(value: CFStringRef) -> Option<String> {
    // SAFETY: callers pass a live CF object; the type is checked before use as a string.
    unsafe {
        if value.is_null() || CFGetTypeID(value) != CFStringGetTypeID() {
            return None;
        }
        let len = CFStringGetLength(value);
        let cap = CFStringGetMaximumSizeForEncoding(len, CF_STRING_ENCODING_UTF8) + 1;
        let mut buf = vec![0 as c_char; cap.max(1) as usize];
        if !CFStringGetCString(value, buf.as_mut_ptr(), cap, CF_STRING_ENCODING_UTF8) {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

fn cfnumber_i32(value: CFNumberRef) -> Option<i32> {
    // SAFETY: type-checked before reading into an i32.
    unsafe {
        if value.is_null() || CFGetTypeID(value) != CFNumberGetTypeID() {
            return None;
        }
        let mut out = 0i32;
        CFNumberGetValue(value, CF_NUMBER_SINT32, (&mut out as *mut i32).cast()).then_some(out)
    }
}

/// Other apps' normal-layer windows (on screen or not), with the owner's bundle id and the title.
/// Titles are `None` when the system withholds them (no Screen Recording permission). Our own
/// windows are skipped: their titles are always readable and would hide a missing permission.
pub(crate) fn app_windows(own_pid: i32) -> Vec<WindowInfo> {
    let mut bundles: HashMap<i32, Option<String>> = HashMap::new();
    let mut out = Vec::new();
    // SAFETY: the returned array is owned by us and released below; its dictionaries and values
    // are borrowed from it and only read while it is alive.
    unsafe {
        let list = CGWindowListCopyWindowInfo(WINDOW_LIST_ALL | WINDOW_LIST_EXCLUDE_DESKTOP, 0);
        if list.is_null() {
            return out;
        }
        for i in 0..CFArrayGetCount(list) {
            let dict = CFArrayGetValueAtIndex(list, i);
            let Some(pid) = cfnumber_i32(CFDictionaryGetValue(dict, kCGWindowOwnerPID)) else {
                continue;
            };
            let layer = cfnumber_i32(CFDictionaryGetValue(dict, kCGWindowLayer)).unwrap_or(0);
            if layer != 0 || pid == own_pid {
                continue;
            }
            let bundle = bundles.entry(pid).or_insert_with(|| bundle_id_of(pid));
            let Some(owner_bundle) = bundle.clone() else {
                continue;
            };
            let title = cfstring_to_string(CFDictionaryGetValue(dict, kCGWindowName))
                .filter(|t| !t.is_empty());
            out.push(WindowInfo {
                owner_bundle,
                title,
            });
        }
        CFRelease(list);
    }
    out
}

/// The bundle id of a running app, via `NSRunningApplication`.
fn bundle_id_of(pid: i32) -> Option<String> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: plain AppKit class/instance messages; nil results are checked.
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid];
            if app.is_null() {
                return None;
            }
            let id: *mut AnyObject = msg_send![app, bundleIdentifier];
            if id.is_null() {
                return None;
            }
            let utf8: *const c_char = msg_send![id, UTF8String];
            (!utf8.is_null()).then(|| CStr::from_ptr(utf8).to_string_lossy().into_owned())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hardware probe (manual): prints the Core Audio process list on this Mac.
    /// Run with `cargo test --lib meeting::probe -- --ignored --nocapture`.
    #[test]
    #[ignore = "prints this machine's audio processes; run by hand on a Mac"]
    fn print_audio_processes() {
        match audio_processes() {
            Ok(list) => {
                println!("{} audio process objects", list.len());
                for (p, running) in list {
                    println!("pid {:>6}  input={running}  {}", p.pid, p.bundle_id);
                }
            }
            Err(_) => println!("Core Audio process list unsupported"),
        }
        let windows = app_windows(std::process::id() as i32);
        let titled = windows.iter().filter(|w| w.title.is_some()).count();
        println!("{} windows, {titled} with titles", windows.len());
    }
}
