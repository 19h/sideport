//! Direct Apple Silicon provisioning UDID lookup, matching Sideloadly's `get_m1_udid`.

use crate::{Error, Result};
use std::ffi::{CStr, c_char, c_void};
use std::ptr::{NonNull, null};

const UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: *const c_void, text: *const c_char, encoding: u32) -> *const c_void;
    fn CFStringGetCString(string: *const c_void, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFRelease(value: *const c_void);
}

#[link(name = "MobileGestalt")]
unsafe extern "C" {
    fn MGCopyAnswer(key: *const c_void) -> *const c_void;
}

struct CfValue(NonNull<c_void>);

impl CfValue {
    fn new(value: *const c_void) -> Option<Self> {
        NonNull::new(value.cast_mut()).map(Self)
    }

    fn as_ptr(&self) -> *const c_void {
        self.0.as_ptr()
    }
}

impl Drop for CfValue {
    fn drop(&mut self) {
        // SAFETY: `CfValue` owns a non-null value returned by a Core Foundation Create/Copy call.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

pub(super) fn provisioning_udid() -> Result<String> {
    // SAFETY: Core Foundation accepts a null allocator and the static NUL-terminated UTF-8 key.
    let key = unsafe { CFStringCreateWithCString(null(), c"ProvisioningUniqueDeviceID".as_ptr(), UTF8) };
    let key = CfValue::new(key).ok_or_else(|| Error::System("MobileGestalt key creation failed".into()))?;

    // SAFETY: `key` is a live CFString; MGCopyAnswer returns an owned CF object or null.
    let answer = unsafe { MGCopyAnswer(key.as_ptr()) };
    let answer = CfValue::new(answer).ok_or_else(|| Error::System("no provisioning UDID".into()))?;

    // SAFETY: `answer` is a live CF object; the type check precedes its use as a CFString.
    if unsafe { CFGetTypeID(answer.as_ptr()) } != unsafe { CFStringGetTypeID() } {
        return Err(Error::System("provisioning UDID is not a string".into()));
    }

    let mut buffer = [0i8; 256];

    // SAFETY: `answer` is a CFString and `buffer` has the supplied capacity in bytes.
    let converted = unsafe { CFStringGetCString(answer.as_ptr(), buffer.as_mut_ptr(), buffer.len() as isize, UTF8) };

    if converted == 0 {
        return Err(Error::System("provisioning UDID conversion failed".into()));
    }

    // SAFETY: CFStringGetCString succeeded and wrote a NUL-terminated string into `buffer`.
    let udid = unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .map_err(|_| Error::System("provisioning UDID is not UTF-8".into()))?;

    if udid.is_empty() || !udid.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-') {
        return Err(Error::System("invalid provisioning UDID".into()));
    }

    Ok(udid.to_owned())
}
