// The glue blaze appends to a proc-macro crate's root file when it builds the crate for
// `wasm32-wasip1` as a WASI reactor: the module's side of the host's ABI (`src/wasm/abi.rs`).
// Not compiled into blaze: it is source text for the macro crate, put inside the module
// `__blaze_proc_macros` after its list of macros (`src/wasm/glue.rs`), so `super` is that
// module. It is written for every edition a macro crate may have, and allows no lint a crate
// may forbid (`unsafe_code`): a crate's `#![forbid]` of one would make its `allow` an error.
// Every edition means 2015 and 2018 too, whose prelude has no `TryFrom`: a trait the 2015
// prelude lacks is named by its path
// (`<usize as __blaze_std::convert::TryFrom<isize>>::try_from`), never called as
// `usize::try_from`, which resolves to nothing there (serde_derive 1.0.214, seq-macro,
// zerocopy-derive 0.7).
//
// It is written for a `#![no_std]` macro crate too (multiversion_no_op), whose extern prelude
// has no `std` and whose prelude has no `Vec`, `String` or `Box`: std is reached only through
// `__blaze_std`, the `extern crate std` of `__blaze_proc_macros`, never as `std` or `::std`,
// and std's types the core prelude lacks are imported from it by name.
//
// The bridge's `Buffer`, `Closure` and `BridgeConfig` are private to `proc_macro`; their
// `#[repr(C)]` layouts are mirrored here, as blaze's server checks them against the pinned
// sources (`src/protocol.rs`). A `Buffer` this glue makes has this glue's own `reserve` and
// `drop`, which the client calls through the buffer's pointers.

#[doc(hidden)]
#[allow(warnings, clippy::all)]
pub mod __blaze_wasm_entry {
    use super::{
        __blaze_std::{self, boxed::Box, mem::ManuallyDrop, panic, string::String, vec::Vec},
        listed,
    };

    #[link(wasm_import_module = "blaze_proc_macro")]
    unsafe extern "C" {
        /// The request of `len` bytes at `request`, answered: the reply's length.
        fn dispatch(request: *const u8, len: usize) -> usize;
        /// The last reply's bytes, written at `into`.
        fn reply(into: *mut u8);
        /// What the run gave, or the panic the hook saw.
        fn output(bytes: *const u8, len: usize);
        /// The variable of the `len`-byte name at `name`, written at `into` if it fits in
        /// `capacity`: its length, or -1 when it is unset.
        fn env_var(name: *const u8, len: usize, into: *mut u8, capacity: usize) -> isize;
        /// The directory cargo runs rustc in for the calling crate, written at `into` if it
        /// fits in `capacity`: its length, or -1 when the host has none.
        fn current_dir(into: *mut u8, capacity: usize) -> isize;
    }

    /// Enters the directory cargo runs rustc in for the calling crate, as rustc's process is
    /// in it: wasi-libc makes a relative path absolute against its own working directory, `/`
    /// until a `chdir`, before the host sees it. That directory is wasi-libc's own state in this
    /// instance's memory: WASI preview 1 has no call that changes one, so the host's process
    /// never moves. With none named, the macro stays in `/`. When the host named one and it
    /// could not be entered: the `errno` `set_current_dir` failed with (wasi-libc's are WASI's
    /// numbers), 0 when it gave none or the name is not UTF-8.
    fn enter_working_directory() -> Result<(), u32> {
        let mut dir = Vec::<u8>::new();
        loop {
            let len = unsafe { current_dir(dir.as_mut_ptr(), dir.capacity()) };
            let Ok(len) = <usize as __blaze_std::convert::TryFrom<isize>>::try_from(len) else { return Ok(()) };
            if len <= dir.capacity() {
                unsafe { dir.set_len(len) };
                break;
            }
            dir.reserve_exact(len);
        }
        let dir = String::from_utf8(dir).map_err(|_| 0u32)?;
        __blaze_std::env::set_current_dir(dir).map_err(|error| error.raw_os_error().map_or(0, |errno| errno as u32 & 0xFF_FFFF))
    }

    /// libc's `getenv`, linked in place of wasi-libc's, which reads every variable through
    /// `environ_get` on its first call: this one asks the host for the one variable, so an
    /// expansion is keyed by the variables it read alone. std's `env::var` calls it. The value
    /// is never freed, as C's `getenv` gives a pointer that outlives the call, and an instance
    /// lives for one expansion.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn getenv(name: *const __blaze_std::ffi::c_char) -> *mut __blaze_std::ffi::c_char {
        let name = unsafe { __blaze_std::ffi::CStr::from_ptr(name) }.to_bytes();
        let mut value = Vec::<u8>::new();
        loop {
            let len = unsafe { env_var(name.as_ptr(), name.len(), value.as_mut_ptr(), value.capacity()) };
            let Ok(len) = <usize as __blaze_std::convert::TryFrom<isize>>::try_from(len) else { return __blaze_std::ptr::null_mut() };
            if len <= value.capacity() {
                unsafe { value.set_len(len) };
                break;
            }
            value.reserve_exact(len);
        }
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
        __blaze_std::mem::drop(unsafe { buffer.into_vec() });
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

    #[repr(C)]
    struct Client {
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

    /// The bridge's encoding of `Err(PanicMessage::String(message))`: `Err`, `Some`, the length
    /// in eight bytes (the bridge pads a 32-bit `usize` to `MAX_USIZE_SIZE`), the bytes.
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

    /// Runs the macro at `index` of the list on the `len` bytes at `input`
    /// ([`blaze_proc_macro_alloc`]'s), in the directory cargo runs rustc in, and gives the host
    /// its output: 0, 1 for no such macro, or 2 when that directory could not be entered, with
    /// the `errno` of why in the bits above the low byte.
    #[unsafe(no_mangle)]
    pub extern "C" fn blaze_proc_macro_expand(index: u32, input: *mut u8, len: usize) -> u32 {
        let input = unsafe { Vec::from_raw_parts(input, len, len) };
        let Some(client) = listed(index) else {
            return 1;
        };
        if let Err(errno) = enter_working_directory() {
            return 2 | errno << 8;
        }
        panic::set_hook(Box::new(hook));
        // SAFETY: `Client` is the bridge's `#[repr(C)]` struct of one `extern "C" fn` pointer.
        let client: Client = unsafe { __blaze_std::mem::transmute(client) };
        let config = Config {
            input: Buffer::of(input),
            dispatch: Closure { call, env: __blaze_std::ptr::null_mut() },
            force_show_panics: true,
        };
        let out = unsafe { (client.run)(config).into_vec() };
        unsafe { output(out.as_ptr(), out.len()) };
        0
    }
}
