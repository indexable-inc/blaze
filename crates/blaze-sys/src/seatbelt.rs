//! macOS calls for a hermetic child (D177, `Level::Inputs` on macOS): Seatbelt, a process
//! group's kill and a child's resource use. Rule 45: the private calls are named as such, with
//! why, and the test below looks every symbol up.
//!
//! - `sandbox_init_with_parameters` (PRIVATE: exported by libsystem_sandbox, part of libSystem,
//!   declared in no SDK header; the public `sandbox_init` takes only Apple's named profiles,
//!   deprecated since 10.8, and no profile text): applies an SBPL profile, given as text, to the
//!   CALLING process for good, its later children included. It is the call `sandbox-exec -p`
//!   makes before it `execve`s. blaze's backend wants it in the child between `fork` and
//!   `execve`; blaze-core's start is `posix_spawn` (no code of blaze's runs there), so today the
//!   backend runs `/usr/bin/sandbox-exec -p <profile> -- <program>` and this call is declared
//!   for the start that will run it in the child (the seam is in the D177 lane's message).
//! - `sandbox_free_error` (PRIVATE, same library): frees the error text the call above returns.
//! - `killpg` (public, signal.h): a signal to every process of a group; the hermetic child is
//!   started as its group's leader, so its deadline or budget stop reaches every helper.
//! - `proc_pid_rusage` (public, libproc.h): another process's `rusage_info_v4`, the budget's
//!   meter on macOS, which has no kernel CPU or memory cap a parent can set on a child
//!   (`RLIMIT_AS` is not enforced by xnu; `RLIMIT_CPU` is, but only once set in the child).
//! - `proc_listpgrppids` (public, libproc.h): enumerates live PIDs in a process group so the
//!   meter covers helpers spawned by the confined program as well as the group leader.

use core::ffi::{CStr, c_char, c_int, c_void};

use crate::Errno;
use crate::macos::RusageInfoV4;

/// Every symbol this module declares, with why and whether Apple declares it.
pub const SYMBOLS: &[(&CStr, crate::macos::Api, &str)] = &[
    (
        c"sandbox_init_with_parameters",
        crate::macos::Api::Private,
        "a hermetic child's Seatbelt profile, given as text, applied in the child before exec \
         (the call sandbox-exec makes); public sandbox_init takes no profile text",
    ),
    (c"sandbox_free_error", crate::macos::Api::Private, "frees sandbox_init_with_parameters' error text"),
    (c"killpg", crate::macos::Api::Public, "a hermetic run's deadline or budget stop sent to its whole group"),
    (c"proc_pid_rusage", crate::macos::Api::Public, "a hermetic child's CPU time and footprint, its budget's meter"),
    (
        c"proc_listpgrppids",
        crate::macos::Api::Public,
        "enumerates live PIDs in a hermetic child's process group so metering covers helpers too",
    ),
];

mod ffi {
    use core::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        pub fn sandbox_init_with_parameters(
            profile: *const c_char,
            flags: u64,
            parameters: *const *const c_char,
            error: *mut *mut c_char,
        ) -> c_int;
        pub fn sandbox_free_error(error: *mut c_char);
        pub fn killpg(group: c_int, signal: c_int) -> c_int;
        pub fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
        pub fn __error() -> *mut c_int;
    }
}

fn last_errno() -> Errno {
    // SAFETY: `__error` returns this thread's live `errno`.
    Errno::from_raw(unsafe { *ffi::__error() } as u16)
}

/// `killpg(group, signal)`: `signal` to every process of `group`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn kill_group(group: i32, signal: i32) -> Result<(), Errno> {
    // SAFETY: two numbers, no memory.
    if unsafe { ffi::killpg(group, signal) } != 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// `proc_pid_rusage(pid, RUSAGE_INFO_V4)`: what process `pid` has used since it began (its CPU
/// time in Mach absolute units, its footprint and its most footprint in bytes).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn child_usage(pid: i32) -> Result<RusageInfoV4, Errno> {
    const INFO_V4: c_int = 4;
    let mut info = RusageInfoV4 { uuid: [0; 16], words: [0; 35] };
    // SAFETY: `RUSAGE_INFO_V4` writes one `struct rusage_info_v4` (blaze-sys macos.rs's layout).
    if unsafe { ffi::proc_pid_rusage(pid, INFO_V4, (&raw mut info).cast::<c_void>()) } != 0 {
        return Err(last_errno());
    }
    Ok(info)
}

/// `proc_listpgrppids(group, out, size_of_val(out))`: fills `out` with the live PIDs in process
/// group `group` and returns how many PIDs were written (`0` when no process remains in `group`).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn group_pids(group: i32, out: &mut [i32]) -> Result<usize, Errno> {
    if out.is_empty() {
        return Ok(0);
    }
    // `proc_listpgrppids` takes the buffer length in bytes and returns a count of PIDs (or -1).
    let bytes = c_int::try_from(core::mem::size_of_val(out)).unwrap_or(c_int::MAX & !3);
    // SAFETY: `out` is a live writable slice of `pid_t` (`i32`) of at least `bytes` bytes;
    // `proc_listpgrppids` writes at most `bytes / size_of::<i32>()` PIDs into `out` and returns
    // the count written, or -1 on error with `errno` set.
    let count = unsafe { crate::macos::cold::proc_listpgrppids(group, out.as_mut_ptr().cast::<c_void>(), bytes) };
    if count < 0 {
        return Err(last_errno());
    }
    let written = usize::try_from(count).unwrap_or(0);
    Ok(written.min(out.len()))
}

/// Why Seatbelt refused a profile: its compiler's first line, copied into the caller's buffer
/// (no allocation: the library's text is freed before this returns), cut at the buffer's end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refused<'b> {
    pub said: &'b [u8],
}

/// `sandbox_init_with_parameters(profile, 0, {NULL}, &error)`: `profile` (SBPL text, NUL-ended)
/// applied to the CALLING process and everything it later runs, for good. Called in a child
/// before `execve`: only this call, no allocation of blaze's (the library allocates on its own
/// heap, which is why the child must be a fresh `fork` of a single-threaded point or the
/// launcher's start, never a thread of a running build: rule 27).
///
/// # Safety
///
/// The caller is a child between `fork` and `execve`, or a process that means to be confined
/// for the rest of its life.
pub unsafe fn apply_to_self<'b>(profile: &CStr, why: &'b mut [u8]) -> Result<(), Refused<'b>> {
    let parameters: [*const c_char; 1] = [core::ptr::null()];
    let mut error: *mut c_char = core::ptr::null_mut();
    // SAFETY: a live NUL-ended profile, an empty NULL-ended parameter list, and a pointer the
    // library writes its error text to (or leaves null).
    let said = unsafe { ffi::sandbox_init_with_parameters(profile.as_ptr(), 0, parameters.as_ptr(), &raw mut error) };
    if said == 0 {
        return Ok(());
    }
    let mut len = 0;
    if !error.is_null() {
        // SAFETY: the library's error is NUL-ended text it owns until `sandbox_free_error`.
        let text = unsafe { CStr::from_ptr(error) }.to_bytes();
        len = text.len().min(why.len());
        why[..len].copy_from_slice(&text[..len]);
        // SAFETY: the error the call above returned, freed once.
        unsafe { ffi::sandbox_free_error(error) };
    }
    Err(Refused { said: &why[..len] })
}

#[cfg(test)]
mod tests {
    use core::ffi::{c_char, c_void};

    use super::SYMBOLS;

    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    /// `RTLD_DEFAULT`.
    const DEFAULT: *mut c_void = -2isize as *mut c_void;

    /// Rule 45: each call this module declares is exported on this macOS.
    #[test]
    fn every_seatbelt_symbol_exists() {
        for (name, api, why) in SYMBOLS {
            // SAFETY: `dlsym` reads a live NUL-ended name and returns an address or null.
            let found = unsafe { dlsym(DEFAULT, name.as_ptr()) };
            assert!(!found.is_null(), "libSystem has no `{name:?}` ({api:?}); blaze uses it for: {why}");
        }
    }

    #[test]
    fn group_pids_lists_current_process_and_rusage_accessors_sum_self_and_children() {
        // SAFETY: `getpid` and `getpgrp` take no pointers and cannot fail.
        let (pid, pgrp) = unsafe { (libc::getpid(), libc::getpgrp()) };
        let mut buf = [0i32; 64];
        let count = super::group_pids(pgrp, &mut buf).expect("proc_listpgrppids for own group");
        assert!(count >= 1, "expected at least one PID in own process group {pgrp}");
        assert!(buf[..count].contains(&pid), "expected own pid {pid} in {:?}", &buf[..count]);

        let empty = super::group_pids(i32::MAX, &mut buf).expect("missing group returns 0");
        assert_eq!(empty, 0);

        let mut info = crate::macos::RusageInfoV4 { uuid: [0; 16], words: [0; 35] };
        info.words[crate::macos::RusageInfoV4::USER] = 10;
        info.words[crate::macos::RusageInfoV4::SYSTEM] = 20;
        info.words[crate::macos::RusageInfoV4::CHILD_USER] = 30;
        info.words[crate::macos::RusageInfoV4::CHILD_SYSTEM] = 40;
        info.words[crate::macos::RusageInfoV4::FOOTPRINT] = 4096;
        assert_eq!(info.self_cpu_ticks(), 30);
        assert_eq!(info.child_cpu_ticks(), 70);
        assert_eq!(info.total_cpu_ticks(), 100);
        assert_eq!(info.footprint(), 4096);
    }
}
