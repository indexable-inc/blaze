//! The C runtime of blaze's `wasm64-blaze-wasip1` target, written in Rust.
//!
//! wasi-libc is C and has no memory64 build for the `wasi_snapshot_preview1_64` imports, so on
//! this target this crate stands in for it: it defines, `#[no_mangle] extern "C"`, every libc
//! symbol that std, its dependencies and a Rust program reach on a WASI preview 1 target, over the
//! imports the `wasip1` crate declares (the one place their module name and ABI are written down),
//! and, for what the seed's host gives only as calls of its own (every change to the file system,
//! positioned reads and writes, syncs), over the `blaze_seed` module's ([`rt::seed`]).
//! std depends on it for that target only (library/std/Cargo.toml) and names it with `extern
//! crate` so it is linked into every program; `wasm32-wasip1` keeps linking the real wasi-libc.
//!
//! What it ports, and from where (wasi-libc d8611876a5d7, its cloudlibc and musl parts):
//! - the entry point, `exit`/`_Exit`/`abort`, `atexit` (crt1-command.c, `__wasm_call_dtors`);
//! - `malloc` and friends over the dlmalloc crate, which grows memory with `memory.grow`;
//! - `errno` and `strerror`/`strerror_r` (musl's messages);
//! - the environment (`environ`, `getenv`, `setenv`, `unsetenv`, `__wasilibc_get_environ`);
//! - preopened directories and the current directory: every path is made absolute against the
//!   cwd and resolved to the longest matching preopen (preopens.c, chdir.c, at_fdcwd.c);
//! - files, directories and links (`open`/`openat`, `stat` family, `readdir` family, `realpath`,
//!   ...), descriptors (`read`/`write`/`pread`/`lseek`/`fcntl`/`isatty`, ...), clocks and sleep;
//! - the pthread calls single-threaded std makes (keys, attributes; creating a thread is
//!   `ENOTSUP`);
//! - entropy for the `getrandom` crate's `custom` backend, which this target selects
//!   ([`rt::random`]);
//! - `memcpy`/`memmove`/`memset`/`memcmp`/`bcmp`/`strlen`, which compiler_builtins leaves to
//!   libc on WASI, with compiler_builtins' own implementation.
//!
//! What WASI can't do fails with a POSIX error (`ENOSYS`, `ENOTSUP`, `EINVAL`) and `errno` set,
//! never a quiet success. The target has no threads (preview 1 without the threads proposal), so
//! the runtime's state lives in plain statics that one thread touches; see [`Global`].

#![no_std]
#![no_builtins]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(non_upper_case_globals)]
#![allow(internal_features)]
#![cfg_attr(all(target_os = "wasi", target_env = "p1"), feature(thread_local))]
#![cfg_attr(all(target_os = "wasi", target_env = "p1", target_arch = "wasm64"), feature(simd_wasm64))]

#[cfg(all(target_os = "wasi", target_env = "p1"))]
mod rt {
    pub(crate) mod dir;
    pub(crate) mod env;
    pub(crate) mod errno;
    pub(crate) mod fd;
    pub(crate) mod fs;
    pub(crate) mod malloc;
    pub(crate) mod mem;
    pub(crate) mod path;
    pub(crate) mod pthread;
    pub(crate) mod random;
    pub(crate) mod seed;
    pub(crate) mod start;
    pub(crate) mod time;
}

/// compiler_builtins' `memcpy`/`memmove`/`memset`/`memcmp`/`strlen` loops, shared rather than
/// copied: compiler_builtins only exports them where it is the C library (its `mem` feature,
/// which its build script turns off for WASI).
#[cfg(all(target_os = "wasi", target_env = "p1"))]
#[path = "../../compiler-builtins/compiler-builtins/src/mem/impls.rs"]
#[allow(unsafe_op_in_unsafe_fn, dead_code)]
mod impls;

/// What [`impls`] takes from compiler_builtins' crate root.
#[cfg(all(target_os = "wasi", target_env = "p1"))]
mod support {
    #[inline(always)]
    pub fn cold_path() {
        core::hint::cold_path();
    }
}

#[cfg(all(target_os = "wasi", target_env = "p1"))]
pub(crate) use imp::*;

#[cfg(all(target_os = "wasi", target_env = "p1"))]
mod imp {
    use core::cell::UnsafeCell;
    use core::ffi::{c_char, c_int};

    /// A static of the runtime. Sound because the target runs one thread: preview 1 has no
    /// threads, `pthread_create` refuses, and WASI delivers no signals, so nothing re-enters.
    pub(crate) struct Global<T>(UnsafeCell<T>);

    // SAFETY: one thread only, see above.
    unsafe impl<T> Sync for Global<T> {}

    impl<T> Global<T> {
        pub(crate) const fn new(value: T) -> Self {
            Global(UnsafeCell::new(value))
        }

        pub(crate) fn get(&self) -> *mut T {
            self.0.get()
        }
    }

    /// Sets `errno` and returns the C failure value, -1.
    #[inline]
    pub(crate) unsafe fn fail(error: c_int) -> c_int {
        crate::rt::errno::set(error);
        -1
    }

    /// [`fail`] for functions that return `ssize_t` or `off_t`.
    #[inline]
    pub(crate) unsafe fn fail_long(error: c_int) -> i64 {
        crate::rt::errno::set(error);
        -1
    }

    /// The result of a raw WASI import: 0 is success, anything else the errno.
    #[inline]
    pub(crate) fn ok(r: i32) -> Result<(), c_int> {
        if r == 0 { Ok(()) } else { Err(r) }
    }

    /// The bytes of a NUL-terminated string, without the NUL. The caller picks the lifetime: the
    /// string must outlive it.
    #[inline]
    pub(crate) unsafe fn cstr<'a>(s: *const c_char) -> &'a [u8] {
        core::slice::from_raw_parts(s.cast::<u8>(), crate::rt::mem::strlen(s))
    }

    /// Writes `parts` to stderr and ends the process with `code`: for the few failures before
    /// `main` that have no caller to return an error to (reading the preopens, the environment).
    pub(crate) unsafe fn die(parts: &[&[u8]], code: i32) -> ! {
        for part in parts {
            let mut rest = *part;
            while !rest.is_empty() {
                let iov = wasip1::Ciovec { buf: rest.as_ptr(), buf_len: rest.len() };
                match wasip1::fd_write(2, &[iov]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => rest = &rest[n..],
                }
            }
        }
        wasip1::wasi_snapshot_preview1::proc_exit(code)
    }

    /// `n` in decimal, for [`die`]'s messages.
    pub(crate) fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                return &buf[i..];
            }
        }
    }

    /// sysexits.h's codes, which wasi-libc exits with when the runtime can't start.
    pub(crate) const EX_SOFTWARE: i32 = 70;
    pub(crate) const EX_OSERR: i32 = 71;
}
