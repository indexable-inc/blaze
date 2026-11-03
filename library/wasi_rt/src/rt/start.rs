//! The entry point and process exit (wasi-libc's crt1-command.c, `__wasm_call_dtors`, exit.c).

use core::ffi::c_int;

use wasip1::wasi_snapshot_preview1 as wasi;

use crate::Global;

unsafe extern "C" {
    /// Runs the static constructors; the linker makes it.
    fn __wasm_call_ctors();
    /// The program's `main` taking no arguments, the name wasm gives `int main(void)`; std reads
    /// the arguments with `args_get` itself.
    fn __main_void() -> c_int;
}

static STARTED: Global<bool> = Global::new(false);

/// The command's entry point: constructors, `main`, `atexit` handlers, then `proc_exit` with
/// `main`'s status when it isn't 0 (returning from `_start` exits with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() {
    // A command runs once; a host calling `_start` again would run main on used state.
    if *STARTED.get() {
        trap();
    }
    *STARTED.get() = true;
    crate::rt::env::init();
    __wasm_call_ctors();
    let status = __main_void();
    __wasm_call_dtors();
    if status != 0 {
        wasi::proc_exit(status);
    }
}

/// POSIX guarantees 32 `atexit` registrations.
const ATEXIT_MAX: usize = 32;

struct AtExit {
    funcs: [Option<extern "C" fn()>; ATEXIT_MAX],
    len: usize,
}

static AT_EXIT: Global<AtExit> = Global::new(AtExit { funcs: [None; ATEXIT_MAX], len: 0 });

#[unsafe(no_mangle)]
pub unsafe extern "C" fn atexit(f: extern "C" fn()) -> c_int {
    let table = &mut *AT_EXIT.get();
    if table.len == ATEXIT_MAX {
        return -1;
    }
    table.funcs[table.len] = Some(f);
    table.len += 1;
    0
}

/// Runs the `atexit` handlers, last registered first. There is no stdio to flush: nothing in
/// this runtime buffers output.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wasm_call_dtors() {
    loop {
        // Read the table each time: a handler may register another.
        let table = &mut *AT_EXIT.get();
        if table.len == 0 {
            return;
        }
        table.len -= 1;
        if let Some(f) = table.funcs[table.len].take() {
            f();
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn exit(status: c_int) -> ! {
    __wasm_call_dtors();
    _Exit(status)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Exit(status: c_int) -> ! {
    wasi::proc_exit(status)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _exit(status: c_int) -> ! {
    wasi::proc_exit(status)
}

/// Ends the process abnormally: a wasm trap, as wasi-libc's `abort` does (preview 1 has no
/// signals to raise).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn abort() -> ! {
    trap()
}

#[inline(always)]
fn trap() -> ! {
    #[cfg(target_arch = "wasm64")]
    core::arch::wasm64::unreachable();
    #[cfg(target_arch = "wasm32")]
    core::arch::wasm32::unreachable();
}
