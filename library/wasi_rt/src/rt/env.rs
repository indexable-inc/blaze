//! The environment: read from `environ_get` at start, then changed by `setenv`/`unsetenv`
//! (wasi-libc's `__wasilibc_initialize_environ`, musl's getenv.c, setenv.c, unsetenv.c).
//!
//! It is read in [`init`], before constructors, rather than on first use as wasi-libc does, so
//! `environ` is right for any code that reads it directly.

use core::ffi::{c_char, c_int};
use core::ptr;

use libc::{EINVAL, ENOMEM};
use wasip1::wasi_snapshot_preview1 as wasi;

use crate::rt::malloc::{free, malloc, realloc};
use crate::{EX_OSERR, EX_SOFTWARE, Global, cstr, decimal, die, fail};

#[unsafe(no_mangle)]
pub static mut environ: *mut *mut c_char = ptr::null_mut();

struct Env {
    /// Entries in `environ`, not counting its terminating null.
    len: usize,
    /// The block `environ_get` filled. Strings inside it are not ours to free; the ones `setenv`
    /// made are.
    block: *mut u8,
    block_len: usize,
}

static ENV: Global<Env> = Global::new(Env { len: 0, block: ptr::null_mut(), block_len: 0 });

pub(crate) unsafe fn init() {
    let (mut count, mut size) = (0usize, 0usize);
    if let Err(e) = crate::ok(wasi::environ_sizes_get(
        (&raw mut count) as usize,
        (&raw mut size) as usize,
    )) {
        let mut n = [0; 20];
        die(&[b"wasi_rt: environ_sizes_get failed with errno ", decimal(e as u64, &mut n), b"\n"], EX_OSERR);
    }
    let Some(slots) = count.checked_add(1) else {
        die(&[b"wasi_rt: the host reports too many environment variables\n"], EX_SOFTWARE);
    };
    let list = crate::rt::malloc::calloc(slots, size_of::<*mut c_char>()).cast::<*mut c_char>();
    let block = malloc(size.max(1)).cast::<u8>();
    if list.is_null() || block.is_null() {
        die(&[b"wasi_rt: out of memory reading the environment\n"], EX_SOFTWARE);
    }
    if count != 0 {
        if let Err(e) = crate::ok(wasi::environ_get(list as usize, block as usize)) {
            let mut n = [0; 20];
            die(&[b"wasi_rt: environ_get failed with errno ", decimal(e as u64, &mut n), b"\n"], EX_OSERR);
        }
    }
    *list.add(count) = ptr::null_mut();
    *ENV.get() = Env { len: count, block, block_len: size };
    environ = list;
}

/// std reads the environment through this (std/src/sys/env/wasi.rs).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wasilibc_get_environ() -> *mut *mut c_char {
    environ
}

/// Kept for wasi-libc's name; the environment is already read when `main` runs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wasilibc_initialize_environ() {}

/// The length of the name in `name`, which must be non-empty and hold no `=`.
unsafe fn name_len(name: *const c_char) -> Option<usize> {
    if name.is_null() {
        return None;
    }
    let name = cstr(name);
    if name.is_empty() || name.contains(&b'=') { None } else { Some(name.len()) }
}

/// The index in `environ` of the entry for `name`.
unsafe fn find(name: &[u8]) -> Option<usize> {
    let env = &*ENV.get();
    (0..env.len).find(|&i| {
        let entry = cstr(*environ.add(i));
        entry.len() > name.len() && entry[name.len()] == b'=' && &entry[..name.len()] == name
    })
}

/// Frees `entry` if `setenv` made it.
unsafe fn release(entry: *mut c_char) {
    let env = &*ENV.get();
    let start = env.block as usize;
    let at = entry as usize;
    if at < start || at >= start + env.block_len {
        free(entry.cast());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    let name = cstr(name);
    if name.is_empty() || name.contains(&b'=') {
        return ptr::null_mut();
    }
    match find(name) {
        Some(i) => (*environ.add(i)).add(name.len() + 1),
        None => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int {
    let Some(len) = name_len(name) else { return fail(EINVAL) };
    let name = cstr(name);
    let found = find(&name[..len]);
    if found.is_some() && overwrite == 0 {
        return 0;
    }
    let value = cstr(value);
    let entry = malloc(len + 1 + value.len() + 1).cast::<u8>();
    if entry.is_null() {
        return -1;
    }
    ptr::copy_nonoverlapping(name.as_ptr(), entry, len);
    *entry.add(len) = b'=';
    ptr::copy_nonoverlapping(value.as_ptr(), entry.add(len + 1), value.len());
    *entry.add(len + 1 + value.len()) = 0;
    let env = &mut *ENV.get();
    match found {
        Some(i) => {
            let old = *environ.add(i);
            *environ.add(i) = entry.cast();
            release(old);
        }
        None => {
            let grown = realloc(environ.cast(), (env.len + 2) * size_of::<*mut c_char>());
            if grown.is_null() {
                free(entry.cast());
                return fail(ENOMEM);
            }
            environ = grown.cast();
            *environ.add(env.len) = entry.cast();
            *environ.add(env.len + 1) = ptr::null_mut();
            env.len += 1;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn unsetenv(name: *const c_char) -> c_int {
    let Some(len) = name_len(name) else { return fail(EINVAL) };
    let name = &cstr(name)[..len];
    while let Some(i) = find(name) {
        let env = &mut *ENV.get();
        let old = *environ.add(i);
        // Keep the order: shift the rest down over it, the null included.
        ptr::copy(environ.add(i + 1), environ.add(i), env.len - i);
        env.len -= 1;
        release(old);
    }
    0
}
