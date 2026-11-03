//! The pthread calls std makes on a target without threads (wasi-libc's single-threaded
//! pthread stubs), `sched_yield`, and `sysconf`.
//!
//! There is one thread, so a thread-specific value is a plain slot per key, and starting a
//! thread fails with `ENOTSUP` (std turns that into an `io::Error` from `thread::spawn`). Key
//! destructors don't run when the main thread exits, as on wasi-libc and as for any process's
//! main thread that calls `exit`.

use core::ffi::{c_int, c_long, c_void};
use core::ptr;

use libc::{
    EAGAIN, EINVAL, ENOTSUP, ESRCH, _SC_NPROCESSORS_CONF, _SC_NPROCESSORS_ONLN, _SC_PAGESIZE, pthread_attr_t, pthread_key_t,
    pthread_t,
};
use wasip1::wasi_snapshot_preview1 as wasi;

use crate::{Global, fail};

/// musl's `PTHREAD_KEYS_MAX`.
const KEYS_MAX: usize = 128;

struct Keys {
    /// Whether key `i + 1` exists. Key 0 is never handed out: std's lazy keys use 0 as "not yet
    /// made" and would make a second key to avoid it.
    used: [bool; KEYS_MAX],
    values: [*mut c_void; KEYS_MAX],
}

static KEYS: Global<Keys> = Global::new(Keys { used: [false; KEYS_MAX], values: [ptr::null_mut(); KEYS_MAX] });

/// The index of an existing key.
unsafe fn slot(key: pthread_key_t) -> Option<usize> {
    let i = (key as usize).checked_sub(1)?;
    if i < KEYS_MAX && (*KEYS.get()).used[i] { Some(i) } else { None }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_key_create(
    key: *mut pthread_key_t,
    _destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let keys = &mut *KEYS.get();
    match keys.used.iter().position(|used| !used) {
        Some(i) => {
            keys.used[i] = true;
            keys.values[i] = ptr::null_mut();
            *key = (i + 1) as pthread_key_t;
            0
        }
        None => EAGAIN,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_key_delete(key: pthread_key_t) -> c_int {
    match slot(key) {
        Some(i) => {
            (*KEYS.get()).used[i] = false;
            0
        }
        None => EINVAL,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_getspecific(key: pthread_key_t) -> *mut c_void {
    match slot(key) {
        Some(i) => (*KEYS.get()).values[i],
        None => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_setspecific(key: pthread_key_t, value: *const c_void) -> c_int {
    match slot(key) {
        Some(i) => {
            (*KEYS.get()).values[i] = value.cast_mut();
            0
        }
        None => EINVAL,
    }
}

/// musl's `PTHREAD_STACK_MIN`.
const STACK_MIN: usize = 2048;

/// The stack size lives in the attribute's first word; the rest is unused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_init(attr: *mut pthread_attr_t) -> c_int {
    ptr::write_bytes(attr, 0, 1);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_destroy(_attr: *mut pthread_attr_t) -> c_int {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_setstacksize(attr: *mut pthread_attr_t, size: usize) -> c_int {
    if size < STACK_MIN || size > usize::MAX / 4 {
        return EINVAL;
    }
    *attr.cast::<usize>() = size;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_getstacksize(attr: *const pthread_attr_t, size: *mut usize) -> c_int {
    *size = *attr.cast::<usize>();
    0
}

/// No threads on this target: refused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_create(
    _thread: *mut pthread_t,
    _attr: *const pthread_attr_t,
    _start: extern "C" fn(*mut c_void) -> *mut c_void,
    _arg: *mut c_void,
) -> c_int {
    ENOTSUP
}

/// No thread was ever made, so none can be joined or detached.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_join(_thread: pthread_t, _value: *mut *mut c_void) -> c_int {
    ESRCH
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_detach(_thread: pthread_t) -> c_int {
    ESRCH
}

static MAIN_THREAD: u8 = 0;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_self() -> pthread_t {
    (&raw const MAIN_THREAD).cast_mut().cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_equal(a: pthread_t, b: pthread_t) -> c_int {
    c_int::from(a == b)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_yield() -> c_int {
    match wasi::sched_yield() {
        0 => 0,
        e => fail(e),
    }
}

/// The facts std asks for: one processor, 64 KiB wasm pages. Any other name is `EINVAL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sysconf(name: c_int) -> c_long {
    match name {
        _SC_NPROCESSORS_CONF | _SC_NPROCESSORS_ONLN => 1,
        _SC_PAGESIZE => 65536,
        _ => {
            fail(EINVAL);
            -1
        }
    }
}

const _: () = assert!(size_of::<pthread_attr_t>() >= size_of::<usize>());
