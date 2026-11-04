//! Logging and faults for every image (host and modules) through the one flecs they share.
//!
//! A log line is a call to flecs `ecs_log_`: the owner of the world installs the sink (`ecs_os_api.log_`) and the level
//! (`ecs_os_api.log_level_`); den-ecs routes both to Rust, and a module calls the flecs symbol the host exports. A fault (a
//! panic caught in a trampoline, a layout mismatch found at run time) lands in this image's panic slot (`hyperion_panic`).
use crate::sys;

pub const INFO: i32 = 0;
pub const WARN: i32 = 1;
pub const ERROR: i32 = 2;
pub const DEBUG: i32 = 3;
pub const TRACE: i32 = 4;

/// Our level (0 info, 1 warn, 2 error, 3 debug, 4 trace) as a flecs level (error -3, warn -2, info 0, debug 1, trace 2).
#[inline]
pub const fn flecs_level(level: i32) -> i32 {
    match level {
        ERROR => -3,
        WARN => -2,
        DEBUG => 1,
        TRACE => 2,
        _ => 0,
    }
}

/// Would flecs print `level`? One load of `ecs_os_api.log_level_`: callers skip formatting when false.
#[inline]
pub fn enabled(level: i32) -> bool {
    // SAFETY: a plain i32 read of the flecs OS API struct; written only when the world owner sets the level (a torn read is impossible for an aligned i32).
    flecs_level(level) <= unsafe { core::ptr::addr_of!(sys::ecs_os_api.log_level_).read_volatile() }
}

/// Hand one line to flecs with its source location. `file` is NUL terminated (`concat!(file!(), "\0")`).
pub fn log_at(level: i32, file: &'static str, line: u32, msg: &str) {
    if !enabled(level) {
        return;
    }
    const CAP: usize = 1024;
    let mut stack = [0u8; CAP];
    let heap;
    let p = if msg.len() < CAP {
        stack[..msg.len()].copy_from_slice(msg.as_bytes());
        stack.as_ptr()
    } else {
        heap = std::ffi::CString::new(msg.replace('\0', "?")).unwrap_or_default();
        heap.as_ptr().cast::<u8>()
    };
    let f = if file.ends_with('\0') {
        file.as_ptr()
    } else {
        c"?".as_ptr().cast()
    };
    // SAFETY: `p` is NUL terminated (stack buffer zero filled past `msg`, or a CString) and outlives the call; "%s" consumes exactly one
    // `char*` vararg.
    unsafe {
        sys::ecs_log_(
            flecs_level(level),
            f.cast(),
            line as i32,
            c"%s".as_ptr(),
            p.cast::<core::ffi::c_char>(),
        )
    };
}

/// Log without a source location.
pub fn log(level: i32, msg: &str) {
    log_at(level, "\0", 0, msg)
}

/// A fault that is not a panic (a layout mismatch found at run time): logged, recorded in this image's panic slot and the image
/// quarantined (its systems and observers stop running until the module is reloaded).
pub fn fault(what: &str, msg: &str) {
    log(ERROR, &format!("FAULT {what}: {msg}"));
    hyperion_panic::record_fault(what, msg);
}
