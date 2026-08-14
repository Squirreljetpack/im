#![allow(clippy::missing_safety_doc)]
//! Link-time compatibility shims for the prebuilt ONNX Runtime.
//!
//! The prebuilt `onnxruntime` static library shipped with the `ort` crate's
//! `download-binaries` feature is compiled against glibc >= 2.38 (Debian 13 /
//! Ubuntu 24.04 era). On older glibc (e.g. Debian 12, glibc 2.36) the final
//! link fails with undefined `__isoc23_strtoll*` symbols — glibc renamed the
//! C23 variants of the `strtol` family and older libc versions don't export
//! them.
//!
//! These three shims delegate to the C99 libc functions, which is
//! behaviorally equivalent for the JSON number scanning that references
//! them: the only C23 change (no leading-whitespace skip) never applies to
//! the already-trimmed buffers ONNX Runtime passes in. On systems with a
//! newer glibc the symbols are simply never referenced, so the shims are
//! dead code there.

use std::os::raw::{c_char, c_int, c_long, c_longlong};

unsafe extern "C" {
    fn strtol(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long;
    fn strtoll(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_longlong;
    fn strtoull(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> u64;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __isoc23_strtol(
    s: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_long {
    unsafe { strtol(s, endptr, base) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __isoc23_strtoll(
    s: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_longlong {
    unsafe { strtoll(s, endptr, base) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __isoc23_strtoull(
    s: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> u64 {
    unsafe { strtoull(s, endptr, base) }
}
