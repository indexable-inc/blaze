// blaze's shim: the module's side of the host's ABI (`src/host.rs`), linked into every
// proc-macro crate's `wasm32-wasip1` module next to the crate's own code. The crate is not
// changed: the bootstrap builder (scripts/src/bin/proc_macro_modules.rs) builds this file as a
// crate of its own against the same std, and passes its rlib to the linker with each of its
// exports named, so the module keeps them.
//
// The crate's macros are the pinned bridge's `Client`s in the static rustc's proc-macro harness
// makes (`#[rustc_proc_macro_decls] static _DECLS: &[Client]`), which the linker exports as a
// global holding its address (`__rustc_proc_macro_decls_<hash>__`). The host reads that global
// and passes the address to `blaze_proc_macro_expand` with the index of the macro to run.
//
// The bridge's `Buffer`, `Closure`, `BridgeConfig` and `Client` are private to `proc_macro`;
// their `#[repr(C)]` layouts are mirrored here, and `src/protocol.rs`'s tests hold them to the
// pinned sources in `library/proc_macro/src/bridge`. A `Buffer` this shim makes has this shim's
// own `reserve` and `drop`, which the client calls through the buffer's pointers; they free
// with the same global allocator the client's own do.
//
// Moved from crates/blaze-macro-eval/guest/entry.rs (2026-11-16), where it was text appended to
// the crate's root; now it is linked, so the crate's source is never touched.

#![allow(unsafe_code, clippy::missing_safety_doc)]

use std::{ffi::CStr, mem::ManuallyDrop, panic};

#[link(wasm_import_module = "blaze_proc_macro")]
unsafe extern "C" {
    /// The request of `len` bytes at `request`, answered: the reply's length.
    fn dispatch(request: *const u8, len: usize) -> usize;
    /// The last reply's bytes, written at `into`.
    fn reply(into: *mut u8);
    /// What the run gave, or the panic the hook saw.
    fn output(bytes: *const u8, len: usize);
    /// The variable of the `len`-byte name at `name`, written at `into` if it fits in
    /// `capacity`: its length, or -1 when it is unset. The host records the read.
    fn env_var(name: *const u8, len: usize, into: *mut u8, capacity: usize) -> isize;
    /// The directory the macro runs in (its package's, as the host names it), written at
    /// `into` if it fits in `capacity`: its length, or -1 when the host names none.
    fn current_dir(into: *mut u8, capacity: usize) -> isize;
}

/// Asks the host a question whose answer is a byte string of unknown length: `ask(into,
/// capacity)` gives the length, or -1 for none; asked again with room when it did not fit.
fn answer(ask: impl Fn(*mut u8, usize) -> isize) -> Option<Vec<u8>> {
    let mut bytes = Vec::<u8>::new();
    loop {
        let len = ask(bytes.as_mut_ptr(), bytes.capacity());
        let len = usize::try_from(len).ok()?;
        if len <= bytes.capacity() {
            unsafe { bytes.set_len(len) };
            return Some(bytes);
        }
        bytes.reserve_exact(len);
    }
}

/// Enters the directory the host names: wasi-libc makes a relative path absolute against its
/// own working directory, `/` until a `chdir`. That directory is wasi-libc's state in this
/// instance's memory; WASI preview 1 has no call that changes one. When it could not be
/// entered: the `errno` (wasi-libc's are WASI's numbers), 0 when it gave none or the name is
/// not UTF-8.
fn enter_working_directory() -> Result<(), u32> {
    let Some(dir) = answer(|into, capacity| unsafe { current_dir(into, capacity) }) else {
        return Ok(());
    };
    let dir = String::from_utf8(dir).map_err(|_| 0u32)?;
    std::env::set_current_dir(dir).map_err(|error| error.raw_os_error().map_or(0, |errno| errno as u32 & 0xFF_FFFF))
}

/// libc's `getenv`, in place of wasi-libc's, which reads every variable through `environ_get`
/// on its first call: this one asks the host for the one variable, so an expansion is keyed by
/// the variables it read alone. std's `env::var` calls it. The module is linked with
/// `--wrap=getenv`, so every call of `getenv`, libc's own included, comes here whatever the
/// link order. The value is never freed, as C's `getenv` gives a pointer that outlives the
/// call, and an instance lives for one expansion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wrap_getenv(name: *const std::ffi::c_char) -> *mut std::ffi::c_char {
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    let Some(mut value) = answer(|into, capacity| unsafe { env_var(name.as_ptr(), name.len(), into, capacity) }) else {
        return std::ptr::null_mut();
    };
    value.push(0);
    Box::leak(value.into_boxed_slice()).as_mut_ptr().cast()
}

#[repr(C)]
struct Buffer {
    data: *mut u8,
    len: usize,
    capacity: usize,
    reserve: extern "C" fn(Buffer, usize) -> Buffer,
    drop: extern "C" fn(Buffer),
}

impl Buffer {
    fn of(bytes: Vec<u8>) -> Buffer {
        let mut bytes = ManuallyDrop::new(bytes);
        Buffer { data: bytes.as_mut_ptr(), len: bytes.len(), capacity: bytes.capacity(), reserve, drop }
    }

    /// The buffer's bytes as the `Vec` they are.
    unsafe fn into_vec(self) -> Vec<u8> {
        let this = ManuallyDrop::new(self);
        unsafe { Vec::from_raw_parts(this.data, this.len, this.capacity) }
    }
}

extern "C" fn reserve(buffer: Buffer, additional: usize) -> Buffer {
    let mut bytes = unsafe { buffer.into_vec() };
    bytes.reserve(additional);
    Buffer::of(bytes)
}

extern "C" fn drop(buffer: Buffer) {
    std::mem::drop(unsafe { buffer.into_vec() });
}

#[repr(C)]
struct Closure {
    call: extern "C" fn(*mut u8, Buffer) -> Buffer,
    env: *mut u8,
}

#[repr(C)]
struct Config {
    input: Buffer,
    dispatch: Closure,
    force_show_panics: bool,
}

/// The bridge's `Client`: one `extern "C" fn` pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Client {
    run: extern "C" fn(Config) -> Buffer,
}

/// `Closure::call`: the request in `buffer` sent to the host, and its reply put in it.
extern "C" fn call(_env: *mut u8, buffer: Buffer) -> Buffer {
    let mut bytes = unsafe { buffer.into_vec() };
    let len = unsafe { dispatch(bytes.as_ptr(), bytes.len()) };
    bytes.clear();
    bytes.reserve(len);
    unsafe {
        reply(bytes.as_mut_ptr());
        bytes.set_len(len);
    }
    Buffer::of(bytes)
}

/// The bridge's encoding of `Err(PanicMessage::String(message))`: `Err`, `Some`, the length in
/// eight bytes (the bridge pads a 32-bit `usize` to `MAX_USIZE_SIZE`), the bytes.
fn panicked(message: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(10 + message.len());
    bytes.extend_from_slice(&[1, 0]);
    bytes.extend_from_slice(&(message.len() as u64).to_le_bytes());
    bytes.extend_from_slice(message.as_bytes());
    bytes
}

/// Gives the host the message of a panic: `panic = "abort"` traps after the hook, so the
/// client's `catch_unwind` never encodes it. `proc_macro`'s own hook calls this one, as
/// `force_show_panics` asks.
fn hook(info: &panic::PanicHookInfo<'_>) {
    let message = match (info.payload().downcast_ref::<&str>(), info.payload().downcast_ref::<String>()) {
        (Some(it), _) => *it,
        (_, Some(it)) => it.as_str(),
        (None, None) => "a payload that is not a string",
    };
    let bytes = panicked(message);
    unsafe { output(bytes.as_ptr(), bytes.len()) };
}

/// `len` bytes the host writes an input to, for [`blaze_proc_macro_expand`].
#[unsafe(no_mangle)]
pub extern "C" fn blaze_proc_macro_alloc(len: usize) -> *mut u8 {
    let mut bytes = ManuallyDrop::new(Vec::<u8>::with_capacity(len));
    bytes.as_mut_ptr()
}

/// Runs the macro at `index` of the harness's list at `decls` (the address the
/// `__rustc_proc_macro_decls_*` global holds) on the `len` bytes at `input`
/// ([`blaze_proc_macro_alloc`]'s), in the directory the host names, and gives the host its
/// output: 0; 1 for no such macro; 2 when that directory could not be entered, with the
/// `errno` of why in the bits above the low byte.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_proc_macro_expand(decls: *const &'static [Client], index: u32, input: *mut u8, len: usize) -> u32 {
    let input = unsafe { Vec::from_raw_parts(input, len, len) };
    // SAFETY: the host passes the address the harness's exported global holds: a
    // `&'static [Client]` in this module's memory.
    let decls: &'static [Client] = unsafe { *decls };
    let Some(client) = decls.get(index as usize).copied() else {
        return 1;
    };
    if let Err(errno) = enter_working_directory() {
        return 2 | errno << 8;
    }
    panic::set_hook(Box::new(hook));
    let config = Config {
        input: Buffer::of(input),
        dispatch: Closure { call, env: std::ptr::null_mut() },
        force_show_panics: true,
    };
    let out = unsafe { (client.run)(config).into_vec() };
    unsafe { output(out.as_ptr(), out.len()) };
    0
}
