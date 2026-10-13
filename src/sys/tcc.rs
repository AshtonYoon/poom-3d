//! System Audio Recording permission.
//!
//! Without it a tap is still created but delivers silence, and a muting tap
//! still mutes, so the permission has to be checked before tapping. macOS has
//! no public API for that; this asks the private TCC framework, and reports
//! `Unknown` if it isn't there.

use std::ffi::{CStr, c_int, c_void};

use objc2_core_foundation::CFString;

const TCC_PATH: &CStr = c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC";
const SERVICE: &str = "kTCCServiceAudioCapture";

#[derive(Debug, PartialEq, Eq)]
pub enum Permission {
    Authorized,
    Denied,
    /// Not decided yet, or it couldn't be checked: macOS asks on first use.
    Unknown,
}

type Preflight = unsafe extern "C" fn(service: *const c_void, options: *const c_void) -> c_int;

pub fn audio_capture_permission() -> Permission {
    unsafe {
        let handle = libc::dlopen(TCC_PATH.as_ptr(), libc::RTLD_NOW);
        if handle.is_null() {
            return Permission::Unknown;
        }
        let symbol = libc::dlsym(handle, c"TCCAccessPreflight".as_ptr());
        if symbol.is_null() {
            return Permission::Unknown;
        }
        let preflight: Preflight = std::mem::transmute(symbol);

        let service = CFString::from_str(SERVICE);
        match preflight((&*service as *const CFString).cast(), std::ptr::null()) {
            0 => Permission::Authorized,
            1 => Permission::Denied,
            _ => Permission::Unknown,
        }
    }
}
