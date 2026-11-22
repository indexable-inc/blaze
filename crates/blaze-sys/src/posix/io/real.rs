//! The real process's own C entry points as FUNCTION VALUES, for a layer that hands them to code
//! it places and runs in this process: the launcher's compile-time child (D175,
//! `crates/blaze-launcher/src/comptime.rs`'s `Real`), whose own C functions forward to these for
//! what the process itself must do (its `errno` cell, anonymous memory, symbol lookup).
//!
//! Which exist where, under the no-C-library rule (blaze-sys on Linux references no C-library
//! symbol):
//!
//! | entry | macOS | Linux |
//! |---|---|---|
//! | `errno` cell | libSystem's `__error` (the floor: libSystem is always linked) | none: the C library's `__errno_location` is what the rule removes. A compile-time child's C layer owns its own `errno` cell (one per thread it has; it has one) and sets it from the raw call's `-errno`; blaze-sys offers nothing |
//! | `mmap`, `mprotect` | libSystem's, as C function values | none as C function values: the kernel's forms are `crate::linux::{mmap, mprotect}` (typed, the error in the result); the child's C layer wraps them in its own `extern "C"` functions that set its own `errno` |
//! | `dlsym`, `RTLD_DEFAULT` | libSystem's | none: symbol lookup in a loaded C library is the user program's C library, a different question (sys.md, "Wave 4") |
//!
//! So [`Real`] and [`real`] are macOS only.

#[cfg(target_os = "macos")]
use core::ffi::{c_char, c_int, c_void};

#[cfg(target_os = "macos")]
use crate::posix::types::off_t;

/// `int *__error(void)`: the calling thread's `errno` cell.
#[cfg(target_os = "macos")]
pub type ErrnoCellFn = unsafe extern "C" fn() -> *mut c_int;
/// `void *mmap(void *addr, size_t len, int prot, int flags, int fd, off_t offset)`.
#[cfg(target_os = "macos")]
pub type MmapFn = unsafe extern "C" fn(*mut c_void, usize, c_int, c_int, c_int, off_t) -> *mut c_void;
/// `int mprotect(void *addr, size_t len, int prot)`.
#[cfg(target_os = "macos")]
pub type MprotectFn = unsafe extern "C" fn(*mut c_void, usize, c_int) -> c_int;
/// `void *dlsym(void *handle, const char *symbol)`.
#[cfg(target_os = "macos")]
pub type DlsymFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

/// The real process's own entries, as values (macOS).
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
pub struct Real {
    /// libSystem's `__error`.
    /// replaces libc::__error (as a function value)
    pub errno: ErrnoCellFn,
    /// libSystem's `mmap`.
    /// replaces libc::mmap (as a function value)
    pub mmap: MmapFn,
    /// libSystem's `mprotect`.
    /// replaces libc::mprotect (as a function value)
    pub mprotect: MprotectFn,
    /// libSystem's `dlsym`.
    /// replaces libc::dlsym (as a function value)
    pub dlsym: DlsymFn,
    /// `RTLD_DEFAULT`: the handle that searches every image.
    /// replaces libc::RTLD_DEFAULT
    pub default_handle: *mut c_void,
}

/// The real process's entries (macOS): the same libSystem symbols blaze-sys already imports
/// (`tools/ratchets/blaze-sys-libsystem.txt`: `__error`, `mmap`, `mprotect`, `dlsym`), so the
/// imported set does not grow.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn real() -> Real {
    Real {
        errno: crate::macos::ffi::__error,
        mmap: super::mem::on_macos::sys_mem::mmap,
        mprotect: super::mem::on_macos::sys_mem::mprotect,
        dlsym: crate::macos::ffi::dlsym,
        default_handle: crate::posix::thread::RTLD_DEFAULT,
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::real;

    #[test]
    fn the_real_entries_are_libsystems() {
        let real = real();
        // SAFETY: each is libSystem's own function, called as its header says.
        unsafe {
            let cell = (real.errno)();
            assert_eq!(cell, libc::__error());
            let page = (real.mmap)(
                core::ptr::null_mut(),
                16384,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            assert_ne!(page, libc::MAP_FAILED);
            assert_eq!((real.mprotect)(page, 16384, libc::PROT_READ), 0);
            assert_eq!(libc::munmap(page, 16384), 0);
            let found = (real.dlsym)(real.default_handle, c"getpid".as_ptr());
            assert_eq!(found, libc::dlsym(libc::RTLD_DEFAULT, c"getpid".as_ptr()));
        }
    }
}
