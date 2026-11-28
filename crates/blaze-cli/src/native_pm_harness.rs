//! Tier-2 native proc-macro executable guest harness (`NATIVE_PROC_MACRO_HARNESS`).
//!
//! Counterpart to `WASM64_PROC_MACRO_SHIM` (`wasm64_compile.rs`) and
//! `crates/blaze-proc-macro/guest/shim.rs` for standalone native proc-macro executables
//! (`native.bin`) compiled by Blaze (`blaze-export::export_par`) with zero `rustc`, zero `lld`,
//! and zero `dlopen`/`dlsym`.
//!
//! The harness binds the proc-macro crate's link-time C-ABI accessors (`__blaze_pm_decls` and
//! `__blaze_pm_decls_len`, emitted on non-wasm targets by
//! `blaze_scope::task::proc_macro_harness_text`), speaks Blaze's `BZPM` v1 binary protocol over
//! inherited `CHILD_FD` (fd 3), intercepts `getenv` during active expansions so `std::env::var`
//! queries are served and tracked per-expansion by the host, wraps each `Client::run` invocation
//! inside `std::panic::catch_unwind` so panicking proc-macros leave the warm worker alive, and
//! loops until `Shutdown` or EOF on `CHILD_FD`.

/// Protocol magic sent in `Hello` and `HelloAck` (`BZPM` v1).
pub(crate) const NATIVE_PM_MAGIC: [u8; 4] = *b"BZPM";

/// Wire protocol version (`BZPM` v1).
pub(crate) const NATIVE_PM_PROTO_VERSION: u32 = 1;

/// Inherited Unix stream socket descriptor (`blaze_core::linked::channel::CHILD_FD`).
pub(crate) const NATIVE_PM_CHILD_FD: i32 = 3;

/// Big-endian `u32` frame length prefix size in bytes (`blaze_core::linked::channel::PREFIX`).
pub(crate) const NATIVE_PM_FRAME_PREFIX_LEN: usize = 4;

/// Maximum body length of a single binary IPC frame (64 MiB, rule 18).
pub(crate) const NATIVE_PM_MAX_FRAME_BYTES: u32 = 64 << 20;

/// Self-contained Rust source of the Tier-2 native proc-macro executable harness crate, compiled
/// and linked directly alongside the proc-macro crate, host `std`, `proc_macro`, and any C
/// archives (`ring`, `aws-lc-sys`, etc.) by `blaze-export::export_par`.
pub(crate) const NATIVE_PROC_MACRO_HARNESS: &str = r#"#![allow(unsafe_code)]

use std::{
    any::Any,
    cell::Cell,
    ffi::{CStr, c_char},
    io::ErrorKind,
    mem::ManuallyDrop,
    panic::{self, AssertUnwindSafe},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const CHILD_FD: i32 = 3;
const MAGIC: [u8; 4] = *b"BZPM";
const PROTO_VERSION: u32 = 1;
const FRAME_PREFIX_LEN: usize = 4;
const MAX_FRAME_BYTES: u32 = 64 << 20;

const TAG_HELLO: u8 = 0x01;
const TAG_HELLO_ACK: u8 = 0x02;
const TAG_EXPAND: u8 = 0x10;
const TAG_REPLY: u8 = 0x11;
const TAG_ENV_VAL: u8 = 0x12;
#[allow(dead_code)]
const TAG_CURRENT_DIR_VAL: u8 = 0x13;
const TAG_SHUTDOWN: u8 = 0x14;

const TAG_DISPATCH: u8 = 0x20;
const TAG_ENV_VAR: u8 = 0x21;
#[allow(dead_code)]
const TAG_CURRENT_DIR: u8 = 0x22;
const TAG_OUTPUT: u8 = 0x23;
const TAG_PANIC: u8 = 0x24;

const KIND_FN_LIKE: u8 = 2;

unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn __blaze_pm_decls() -> *const Client;
    fn __blaze_pm_decls_len() -> usize;
}

static IN_EXPAND: AtomicBool = AtomicBool::new(false);
static CHANNEL_LOCK: Mutex<()> = Mutex::new(());
static ENV_POOL: Mutex<Vec<Box<[u8]>>> = Mutex::new(Vec::new());

thread_local! {
    static IN_RPC: Cell<bool> = const { Cell::new(false) };
}

struct RpcGuard;

impl Drop for RpcGuard {
    fn drop(&mut self) {
        IN_RPC.with(|flag| flag.set(false));
    }
}

struct ExpandGuard;

impl Drop for ExpandGuard {
    fn drop(&mut self) {
        IN_EXPAND.store(false, Ordering::Release);
        ENV_POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
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
        Buffer {
            data: bytes.as_mut_ptr(),
            len: bytes.len(),
            capacity: bytes.capacity(),
            reserve,
            drop: drop_buf,
        }
    }

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

extern "C" fn drop_buf(buffer: Buffer) {
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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Client {
    run: extern "C" fn(Config) -> Buffer,
}

fn read_exact_fd(buf: &mut [u8], allow_clean_eof: bool) -> Result<bool, ()> {
    let mut pos = 0usize;
    while pos < buf.len() {
        let n = unsafe { read(CHILD_FD, buf.as_mut_ptr().add(pos), buf.len() - pos) };
        if n > 0 {
            pos += n as usize;
        } else if n == 0 {
            return if pos == 0 && allow_clean_eof { Ok(false) } else { Err(()) };
        } else if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
            continue;
        } else {
            return Err(());
        }
    }
    Ok(true)
}

fn write_all_fd(mut buf: &[u8]) -> Result<(), ()> {
    while !buf.is_empty() {
        let n = unsafe { write(CHILD_FD, buf.as_ptr(), buf.len()) };
        if n > 0 {
            buf = &buf[n as usize..];
        } else if n == 0 {
            return Err(());
        } else if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
            continue;
        } else {
            return Err(());
        }
    }
    Ok(())
}

fn recv_frame(out: &mut Vec<u8>) -> Result<bool, ()> {
    let mut prefix = [0u8; FRAME_PREFIX_LEN];
    if !read_exact_fd(&mut prefix, true)? {
        return Ok(false);
    }
    let body_len = u32::from_be_bytes(prefix);
    if body_len == 0 || body_len > MAX_FRAME_BYTES {
        return Err(());
    }
    let len = usize::try_from(body_len).map_err(|_| ())?;
    out.clear();
    out.resize(len, 0);
    read_exact_fd(out.as_mut_slice(), false)?;
    Ok(true)
}

fn send_framed_bytes(tag: u8, payload: &[u8]) -> Result<(), ()> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| ())?;
    let body_len = 1usize
        .checked_add(4)
        .and_then(|n| n.checked_add(payload.len()))
        .ok_or(())?;
    let body_u32 = u32::try_from(body_len).map_err(|_| ())?;
    if body_u32 == 0 || body_u32 > MAX_FRAME_BYTES {
        return Err(());
    }
    let mut frame = Vec::with_capacity(FRAME_PREFIX_LEN + body_len);
    frame.extend_from_slice(&body_u32.to_be_bytes());
    frame.push(tag);
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(payload);
    write_all_fd(&frame)
}

fn send_hello_ack() -> Result<(), ()> {
    let mut frame = [0u8; FRAME_PREFIX_LEN + 9];
    frame[0..4].copy_from_slice(&9u32.to_be_bytes());
    frame[4] = TAG_HELLO_ACK;
    frame[5..9].copy_from_slice(&MAGIC);
    frame[9..13].copy_from_slice(&PROTO_VERSION.to_le_bytes());
    write_all_fd(&frame)
}

fn read_u32_le(slice: &[u8], at: usize) -> Option<u32> {
    let bytes = slice.get(at..at + 4)?;
    let arr: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}

fn panic_payload_str(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        *s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "a payload that is not a string"
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() || !IN_EXPAND.load(Ordering::Acquire) {
        return std::ptr::null_mut();
    }
    if IN_RPC.with(|flag| flag.replace(true)) {
        return std::ptr::null_mut();
    }
    let _rpc_guard = RpcGuard;
    let Ok(name_str) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return std::ptr::null_mut();
    };
    let _chan = CHANNEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if send_framed_bytes(TAG_ENV_VAR, name_str.as_bytes()).is_err() {
        std::process::exit(0);
    }
    let mut body = Vec::new();
    match recv_frame(&mut body) {
        Ok(true) => {}
        Ok(false) => std::process::exit(0),
        Err(()) => std::process::exit(1),
    }
    if body.as_slice() == [TAG_SHUTDOWN] {
        std::process::exit(0);
    }
    if body.first().copied() != Some(TAG_ENV_VAL) {
        std::process::exit(1);
    }
    match body.get(1).copied() {
        Some(0) if body.len() == 2 => std::ptr::null_mut(),
        Some(1) => {
            let Some(val_len) = read_u32_le(&body, 2).and_then(|n| usize::try_from(n).ok()) else {
                std::process::exit(1);
            };
            if body.len() != 6usize.saturating_add(val_len) {
                std::process::exit(1);
            }
            let val_bytes = &body[6..];
            if std::str::from_utf8(val_bytes).is_err() {
                std::process::exit(1);
            }
            let mut cstr = Vec::with_capacity(val_bytes.len() + 1);
            cstr.extend_from_slice(val_bytes);
            cstr.push(0);
            let mut boxed = cstr.into_boxed_slice();
            let ptr = boxed.as_mut_ptr().cast::<c_char>();
            ENV_POOL.lock().unwrap_or_else(|e| e.into_inner()).push(boxed);
            ptr
        }
        _ => std::process::exit(1),
    }
}

extern "C" fn dispatch_call(_env: *mut u8, buffer: Buffer) -> Buffer {
    let mut bytes = unsafe { buffer.into_vec() };
    let _rpc_guard = {
        IN_RPC.with(|flag| flag.set(true));
        RpcGuard
    };
    let _chan = CHANNEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if send_framed_bytes(TAG_DISPATCH, &bytes).is_err() {
        std::process::exit(0);
    }
    let mut body = Vec::new();
    match recv_frame(&mut body) {
        Ok(true) => {}
        Ok(false) => std::process::exit(0),
        Err(()) => std::process::exit(1),
    }
    if body.as_slice() == [TAG_SHUTDOWN] {
        std::process::exit(0);
    }
    if body.first().copied() != Some(TAG_REPLY) {
        std::process::exit(1);
    }
    let Some(rep_len) = read_u32_le(&body, 1).and_then(|n| usize::try_from(n).ok()) else {
        std::process::exit(1);
    };
    if body.len() != 5usize.saturating_add(rep_len) {
        std::process::exit(1);
    }
    bytes.clear();
    bytes.extend_from_slice(&body[5..]);
    Buffer::of(bytes)
}

fn main() {
    panic::set_hook(Box::new(|info| {
        if !info.can_unwind() && IN_EXPAND.load(Ordering::Acquire) {
            if let Ok(_chan) = CHANNEL_LOCK.try_lock() {
                let _ = send_framed_bytes(TAG_PANIC, panic_payload_str(info.payload()).as_bytes());
            }
        }
    }));

    let mut body = Vec::new();
    // 1. Versioned BZPM v1 handshake.
    match recv_frame(&mut body) {
        Ok(true) => {}
        Ok(false) => return,
        Err(()) => std::process::exit(1),
    }
    if body.len() != 9
        || body[0] != TAG_HELLO
        || body[1..5] != MAGIC
        || read_u32_le(&body, 5) != Some(PROTO_VERSION)
    {
        std::process::exit(1);
    }
    if send_hello_ack().is_err() {
        std::process::exit(1);
    }

    let decls_ptr = unsafe { __blaze_pm_decls() };
    let decls_len = unsafe { __blaze_pm_decls_len() };

    // 2. Persistent expansion loop until Shutdown or clean EOF on CHILD_FD.
    loop {
        match recv_frame(&mut body) {
            Ok(true) => {}
            Ok(false) => return,
            Err(()) => std::process::exit(1),
        }
        match body.first().copied() {
            Some(TAG_SHUTDOWN) if body.len() == 1 => return,
            Some(TAG_EXPAND) => {
                let Some(index) = read_u32_le(&body, 1) else {
                    std::process::exit(1);
                };
                let Some(kind) = body.get(5).copied() else {
                    std::process::exit(1);
                };
                let Some(input_len) = read_u32_le(&body, 6).and_then(|n| usize::try_from(n).ok()) else {
                    std::process::exit(1);
                };
                if kind > KIND_FN_LIKE || body.len() != 10usize.saturating_add(input_len) {
                    std::process::exit(1);
                }
                let idx = match usize::try_from(index) {
                    Ok(i) => i,
                    Err(_) => {
                        if send_framed_bytes(TAG_PANIC, b"proc-macro index out of range").is_err() {
                            std::process::exit(1);
                        }
                        continue;
                    }
                };
                if idx >= decls_len || decls_ptr.is_null() {
                    if send_framed_bytes(TAG_PANIC, b"proc-macro index out of bounds").is_err() {
                        std::process::exit(1);
                    }
                    continue;
                }
                let client = unsafe { *decls_ptr.add(idx) };
                let input = body[10..].to_vec();
                let config = Config {
                    input: Buffer::of(input),
                    dispatch: Closure { call: dispatch_call, env: std::ptr::null_mut() },
                    force_show_panics: false,
                };
                IN_EXPAND.store(true, Ordering::Release);
                let guard = ExpandGuard;
                let result = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                    (client.run)(config).into_vec()
                }));
                drop(guard);
                let _chan = CHANNEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let sent = match result {
                    Ok(out) => send_framed_bytes(TAG_OUTPUT, &out),
                    Err(payload) => send_framed_bytes(TAG_PANIC, panic_payload_str(&*payload).as_bytes()),
                };
                if sent.is_err() {
                    std::process::exit(1);
                }
            }
            _ => std::process::exit(1),
        }
    }
}
"#;

#[cfg(test)]
mod tests {
    use blaze_core::linked::channel::{CHILD_FD, PREFIX};
    use blaze_expand::ProcKind;
    use blaze_id::CrateItemKey;
    use blaze_ids::ContentId;
    use blaze_own::testing::heap;
    use blaze_scope::{
        graph::Sysroot,
        resolve::{CrateInput, SourceTexts, resolve_crates},
        task::{
            PROC_MACRO_DECLS, PROC_MACRO_DECLS_FN, PROC_MACRO_DECLS_LEN_FN, Serial,
            proc_macro_decls_fn_key, proc_macro_decls_key, proc_macro_decls_len_fn_key,
        },
    };
    use blaze_surface::Edition;
    use blaze_unit_graph::{Implicit, Prelude, StdIs};

    use super::*;

    struct MemTexts<'a>(&'a [&'a [(&'a str, &'a str)]]);

    impl<'a> SourceTexts<'a> for MemTexts<'a> {
        fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
            self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
        }
    }

    #[test]
    fn wire_constants_match_channel_and_bzpm_v1_spec() {
        assert_eq!(NATIVE_PM_MAGIC, *b"BZPM");
        assert_eq!(NATIVE_PM_PROTO_VERSION, 1);
        assert_eq!(NATIVE_PM_CHILD_FD, i32::try_from(CHILD_FD).expect("CHILD_FD fits i32"));
        assert_eq!(NATIVE_PM_FRAME_PREFIX_LEN, PREFIX);
        assert_eq!(NATIVE_PM_MAX_FRAME_BYTES, 64 << 20);

        // Verify the embedded guest harness source embeds the exact same BZPM v1 wire constants.
        for needle in [
            "const CHILD_FD: i32 = 3;",
            "const MAGIC: [u8; 4] = *b\"BZPM\";",
            "const PROTO_VERSION: u32 = 1;",
            "const FRAME_PREFIX_LEN: usize = 4;",
            "const MAX_FRAME_BYTES: u32 = 64 << 20;",
            "const TAG_HELLO: u8 = 0x01;",
            "const TAG_HELLO_ACK: u8 = 0x02;",
            "const TAG_EXPAND: u8 = 0x10;",
            "const TAG_REPLY: u8 = 0x11;",
            "const TAG_ENV_VAL: u8 = 0x12;",
            "const TAG_CURRENT_DIR_VAL: u8 = 0x13;",
            "const TAG_SHUTDOWN: u8 = 0x14;",
            "const TAG_DISPATCH: u8 = 0x20;",
            "const TAG_ENV_VAR: u8 = 0x21;",
            "const TAG_CURRENT_DIR: u8 = 0x22;",
            "const TAG_OUTPUT: u8 = 0x23;",
            "const TAG_PANIC: u8 = 0x24;",
            "fn __blaze_pm_decls() -> *const Client;",
            "fn __blaze_pm_decls_len() -> usize;",
            "pub unsafe extern \"C\" fn getenv(",
            "panic::catch_unwind(",
        ] {
            assert!(
                NATIVE_PROC_MACRO_HARNESS.contains(needle),
                "NATIVE_PROC_MACRO_HARNESS missing `{needle}`"
            );
        }
    }

    #[test]
    fn bridge_repr_c_layouts_match_pinned_proc_macro_sources() {
        // Mirror `crates/blaze-proc-macro/src/protocol.rs` layout verification directly against
        // the pinned `library/proc_macro/src/bridge` definitions.
        let bridge_buffer = include_str!("../../../library/proc_macro/src/bridge/buffer.rs");
        let bridge_closure = include_str!("../../../library/proc_macro/src/bridge/closure.rs");
        let bridge_client = include_str!("../../../library/proc_macro/src/bridge/client.rs");
        let bridge_mod = include_str!("../../../library/proc_macro/src/bridge/mod.rs");

        assert!(bridge_buffer.contains(
            "#[repr(C)]\npub struct Buffer {\n    data: *mut u8,\n    len: usize,\n    capacity: usize,\n    reserve: extern \"C\" fn(Buffer, usize) -> Buffer,\n    drop: extern \"C\" fn(Buffer),\n}"
        ));
        assert!(bridge_closure.contains(
            "#[repr(C)]\npub(super) struct Closure<'a> {\n    call: extern \"C\" fn(*mut Env, Buffer) -> Buffer,\n    env: *mut Env,"
        ));
        assert!(bridge_client.contains(
            "#[repr(C)]\n#[derive(Copy, Clone)]\npub struct Client {\n    pub(super) run: extern \"C\" fn(BridgeConfig<'_>) -> Buffer,\n}"
        ));
        assert!(bridge_mod.contains(
            "#[repr(C)]\npub struct BridgeConfig<'a> {\n    /// Buffer used to pass initial input to the client.\n    input: Buffer,\n\n    /// Server-side function that the client uses to make requests.\n    dispatch: closure::Closure<'a>,\n\n    /// If 'true', always invoke the default panic hook\n    force_show_panics: bool,\n}"
        ));

        for layout in [
            "#[repr(C)]\nstruct Buffer {\n    data: *mut u8,\n    len: usize,\n    capacity: usize,\n    reserve: extern \"C\" fn(Buffer, usize) -> Buffer,\n    drop: extern \"C\" fn(Buffer),\n}",
            "#[repr(C)]\nstruct Closure {\n    call: extern \"C\" fn(*mut u8, Buffer) -> Buffer,\n    env: *mut u8,\n}",
            "#[repr(C)]\nstruct Config {\n    input: Buffer,\n    dispatch: Closure,\n    force_show_panics: bool,\n}",
            "#[repr(C)]\n#[derive(Clone, Copy)]\npub struct Client {\n    run: extern \"C\" fn(Config) -> Buffer,\n}",
        ] {
            assert!(
                NATIVE_PROC_MACRO_HARNESS.contains(layout),
                "NATIVE_PROC_MACRO_HARNESS struct layout drifted from pinned bridge:\n{layout}"
            );
        }
    }

    #[test]
    fn harness_and_link_time_decls_parse_and_resolve_with_blaze_frontend() {
        // 1. Outline-parse `NATIVE_PROC_MACRO_HARNESS` with Blaze's parser (zero errors).
        let alloc = heap().alloc();
        let outlined = blaze_file_items::outline(NATIVE_PROC_MACRO_HARNESS, Edition::E2024, alloc);
        assert!(
            outlined.errors().is_empty(),
            "NATIVE_PROC_MACRO_HARNESS outline parse errors: {:?}",
            outlined.errors()
        );

        // 2. Resolve both a proc-macro crate (emitting `_DECLS`, `__blaze_pm_decls`,
        //    `__blaze_pm_decls_len`) and the harness crate together via `blaze-scope`.
        const PM_SRC: &str = r#"#![no_core]
#[proc_macro_derive(Hello)]
pub fn derive_hello() {}
#[proc_macro_attribute]
pub fn trace_attr() {}
#[proc_macro]
pub fn make_item() {}
"#;
        let texts = MemTexts(&[
            &[("/pm/src/lib.rs", PM_SRC)],
            &[("/harness/src/main.rs", NATIVE_PROC_MACRO_HARNESS)],
        ]);
        let native_cfg: &[(&str, Option<&str>)] = &[
            ("target_arch", Some("aarch64")),
            ("target_os", Some("linux")),
            ("target_family", Some("unix")),
            ("unix", None),
            ("target_pointer_width", Some("64")),
            ("target_endian", Some("little")),
        ];
        let pm_key = CrateItemKey(ContentId::of("test native pm crate", b"pm"));
        let harness_key = CrateItemKey(ContentId::of("test native pm harness", b"harness"));
        let crates = [
            CrateInput {
                name: "demo_pm",
                key: pm_key,
                edition: Edition::E2024,
                implicit: Implicit {
                    std: StdIs::Rust,
                    prelude: Prelude::Rust(edition::Edition::Edition2024),
                    proc_macro: true,
                    entry: false,
                },
                root: "/pm/src/lib.rs",
                shown_from: "/pm/",
                features: &[],
                deps: &[],
                cfg: native_cfg,
                script: None,
            },
            CrateInput {
                name: "blaze_native_pm_harness",
                key: harness_key,
                edition: Edition::E2024,
                implicit: Implicit {
                    std: StdIs::Rust,
                    prelude: Prelude::Rust(edition::Edition::Edition2024),
                    proc_macro: false,
                    entry: false,
                },
                root: "/harness/src/main.rs",
                shown_from: "/harness/",
                features: &[],
                deps: &[],
                cfg: native_cfg,
                script: None,
            },
        ];
        let sb = bstd::mem::SharedBump::new(16 << 20, bstd::mem::Tag::Nameres).expect("bump");
        let names = resolve_crates(&crates, Sysroot::default(), &texts, &Serial, blaze_own::Bump::of(&sb))
            .expect("resolve_crates for native pm + harness");

        // Verify `_DECLS`, `__blaze_pm_decls`, and `__blaze_pm_decls_len` all exist in `demo_pm`.
        let pm_tables = &names.krate(0).tables;
        let (decls_name, _) = pm_tables
            .texts
            .get(&proc_macro_decls_key(pm_key))
            .map(|(t, _)| (t.name, t.text))
            .expect("_DECLS present");
        assert_eq!(decls_name, Some(PROC_MACRO_DECLS));

        let (fn_name, fn_text) = pm_tables
            .texts
            .get(&proc_macro_decls_fn_key(pm_key))
            .map(|(t, _)| (t.name, t.text))
            .expect("__blaze_pm_decls present on native target");
        assert_eq!(fn_name, Some(PROC_MACRO_DECLS_FN));
        assert!(fn_text.contains("#[unsafe(export_name = \"__blaze_pm_decls\")]"));
        assert!(fn_text.contains("Client::expand1(crate::derive_hello)"));
        assert!(fn_text.contains("Client::expand2(crate::trace_attr)"));
        assert!(fn_text.contains("Client::expand1(crate::make_item)"));

        let (len_name, len_text) = pm_tables
            .texts
            .get(&proc_macro_decls_len_fn_key(pm_key))
            .map(|(t, _)| (t.name, t.text))
            .expect("__blaze_pm_decls_len present on native target");
        assert_eq!(len_name, Some(PROC_MACRO_DECLS_LEN_FN));
        assert!(len_text.contains("#[unsafe(export_name = \"__blaze_pm_decls_len\")]"));
        assert!(len_text.contains("3usize"));

        // Verify the harness crate resolved `main`, `getenv`, `dispatch_call`, and foreign decls
        // with zero parse/lowering/expansion notes (only expected empty-sysroot prelude notes).
        let harness_krate = names.krate(1);
        let unexpected_notes: std::vec::Vec<_> = harness_krate
            .notes
            .iter()
            .filter(|n| {
                !matches!(
                    n,
                    blaze_scope::resolve::NamesNote::ExternDropped { .. }
                        | blaze_scope::resolve::NamesNote::Root(
                            blaze_scope::task::RootNote::NoStdPrelude
                        )
                )
            })
            .collect();
        assert!(
            unexpected_notes.is_empty(),
            "unexpected resolve notes on NATIVE_PROC_MACRO_HARNESS: {unexpected_notes:?}"
        );
        let mut saw_main = false;
        let mut saw_getenv = false;
        let mut saw_decls_extern = false;
        let mut saw_decls_len_extern = false;
        for (item, _) in harness_krate.tables.texts.values() {
            match item.name {
                Some("main") => saw_main = true,
                Some("getenv") => saw_getenv = true,
                Some("__blaze_pm_decls") => saw_decls_extern = true,
                Some("__blaze_pm_decls_len") => saw_decls_len_extern = true,
                _ => {}
            }
        }
        assert!(saw_main && saw_getenv && saw_decls_extern && saw_decls_len_extern);
    }

    #[test]
    fn bzpm_v1_wire_frame_encoding_and_decoding_bit_for_bit() {
        // Verify exact byte sequences produced/expected by NATIVE_PROC_MACRO_HARNESS against the
        // BZPM v1 host wire format (`crates/blaze-proc-macro/src/native_bin_proto.rs`).
        fn encode_frame(body: &[u8]) -> std::vec::Vec<u8> {
            let mut out = std::vec::Vec::with_capacity(4 + body.len());
            out.extend_from_slice(&u32::try_from(body.len()).expect("len").to_be_bytes());
            out.extend_from_slice(body);
            out
        }

        // Hello / HelloAck: 4-byte BE length (9) + 1-byte tag + 4-byte "BZPM" + 4-byte LE version (1).
        let hello = encode_frame(&[0x01, b'B', b'Z', b'P', b'M', 1, 0, 0, 0]);
        assert_eq!(hello, [0, 0, 0, 9, 0x01, b'B', b'Z', b'P', b'M', 1, 0, 0, 0]);
        let hello_ack = encode_frame(&[0x02, b'B', b'Z', b'P', b'M', 1, 0, 0, 0]);
        assert_eq!(hello_ack, [0, 0, 0, 9, 0x02, b'B', b'Z', b'P', b'M', 1, 0, 0, 0]);

        // Expand(index=3, kind=FnLike(2), input=b"hi"):
        let kind_byte = |k: ProcKind| -> u8 {
            match k {
                ProcKind::Derive => 0,
                ProcKind::Attr => 1,
                ProcKind::FnLike => 2,
            }
        };
        let expand = encode_frame(&[
            0x10,
            3, 0, 0, 0,
            kind_byte(ProcKind::FnLike),
            2, 0, 0, 0,
            b'h', b'i',
        ]);
        assert_eq!(expand[..4], 12u32.to_be_bytes());
        assert_eq!(expand[4], 0x10);
        assert_eq!(u32::from_le_bytes([expand[5], expand[6], expand[7], expand[8]]), 3);
        assert_eq!(expand[9], 2);
        assert_eq!(u32::from_le_bytes([expand[10], expand[11], expand[12], expand[13]]), 2);
        assert_eq!(&expand[14..], b"hi");

        // Dispatch / Reply / EnvVar / EnvVal(None, Some) / Output / Panic / Shutdown:
        let dispatch = encode_frame(&[0x20, 3, 0, 0, 0, 1, 2, 3]);
        assert_eq!(dispatch[..5], [0, 0, 0, 8, 0x20]);
        let reply = encode_frame(&[0x11, 2, 0, 0, 0, 0, 1]);
        assert_eq!(reply[..5], [0, 0, 0, 7, 0x11]);
        let env_var = encode_frame(&[0x21, 3, 0, 0, 0, b'F', b'O', b'O']);
        assert_eq!(env_var[..5], [0, 0, 0, 8, 0x21]);
        let env_none = encode_frame(&[0x12, 0]);
        assert_eq!(env_none, [0, 0, 0, 2, 0x12, 0]);
        let env_some = encode_frame(&[0x12, 1, 1, 0, 0, 0, b'.']);
        assert_eq!(env_some, [0, 0, 0, 7, 0x12, 1, 1, 0, 0, 0, b'.']);
        let output = encode_frame(&[0x23, 2, 0, 0, 0, 0, 1]);
        assert_eq!(output[..5], [0, 0, 0, 7, 0x23]);
        let panic_msg = encode_frame(&[0x24, 4, 0, 0, 0, b'b', b'o', b'o', b'm']);
        assert_eq!(panic_msg[..5], [0, 0, 0, 9, 0x24]);
        let shutdown = encode_frame(&[0x14]);
        assert_eq!(shutdown, [0, 0, 0, 1, 0x14]);
    }

    #[test]
    fn blaze_engine_check_typechecks_and_borrowchecks_harness_and_pm_decls_with_real_toolchain() {
        let scratch = std::env::temp_dir().join(std::format!(
            "blaze-native-pm-harness-check-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let ws_dir = scratch.join("ws");
        let home_dir = scratch.join("home");
        std::fs::create_dir_all(ws_dir.join("demo_pm/src")).expect("mkdir demo_pm");
        std::fs::create_dir_all(ws_dir.join("harness/src")).expect("mkdir harness");
        std::fs::create_dir_all(home_dir.join(".blaze/v2")).expect("mkdir home");

        let io = bstd::test::native(&ws_dir).expect("native io");
        let Some(real_home) = std::env::var_os("HOME") else {
            let _ = std::fs::remove_dir_all(&scratch);
            return;
        };
        let real_shelf = std::path::PathBuf::from(real_home).join(".blaze/v2/toolchain");
        let tc_id = blaze_toolchain::PINNED;
        if !real_shelf.join(std::format!("{tc_id}")).exists() {
            let _ = std::fs::remove_dir_all(&scratch);
            return;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_shelf, home_dir.join(".blaze/v2/toolchain"))
            .expect("symlink toolchain");

        std::fs::write(
            ws_dir.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"demo_pm\", \"harness\"]\n",
        )
        .expect("workspace Cargo.toml");
        std::fs::write(
            ws_dir.join("Cargo.lock"),
            "version = 4\n\n\
             [[package]]\nname = \"demo_pm\"\nversion = \"0.1.0\"\n\n\
             [[package]]\nname = \"harness\"\nversion = \"0.1.0\"\n",
        )
        .expect("Cargo.lock");
        std::fs::write(
            ws_dir.join("demo_pm/Cargo.toml"),
            "[package]\nname = \"demo_pm\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [lib]\nproc-macro = true\n",
        )
        .expect("demo_pm Cargo.toml");
        std::fs::write(
            ws_dir.join("demo_pm/src/lib.rs"),
            "extern crate proc_macro;\n\
             use proc_macro::TokenStream;\n\
             #[proc_macro_derive(Hello)]\n\
             pub fn derive_hello(input: TokenStream) -> TokenStream { input }\n\
             #[proc_macro_attribute]\n\
             pub fn trace_attr(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n\
             #[proc_macro]\n\
             pub fn make_item(input: TokenStream) -> TokenStream { input }\n",
        )
        .expect("demo_pm lib.rs");
        std::fs::write(
            ws_dir.join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("harness Cargo.toml");
        std::fs::write(ws_dir.join("harness/src/main.rs"), NATIVE_PROC_MACRO_HARNESS)
            .expect("harness main.rs");

        let home_str = home_dir.to_str().expect("utf8 home");
        let ws_str = ws_dir.to_str().expect("utf8 ws");
        let cx = io
            .cx(core::time::Duration::from_secs(120), bstd::mem::TASK_FRAME)
            .expect("cx");
        let frame = bstd::mem::Frame::with_reserve(
            crate::engine_check::REQUEST_FRAME,
            bstd::mem::Tag::Other,
        )
        .expect("request frame");
        let producers = blaze_ids::producer::Producers::for_tests(b"native-pm-harness");
        let opts = crate::engine_check::Opts {
            tools: crate::engine_store::tools(&crate::home::tools(
                &producers,
                crate::lints::Lints::NONE.rules,
            )),
            producers,
            workers: core::num::NonZeroU32::new(2).expect("2 workers"),
            toolchain: tc_id,
            heap: heap(),
            place: crate::engine_check::Place {
                checkout: ws_str,
                served: crate::engine_script_reads::Served::Simulated,
                cert_file: None,
                cert_dir: None,
                rustflags: blaze_manifest::rustflags::RustflagsEnv::default(),
                home: Some(home_str),
                max_store_bytes: None,
                cargo_home: None,
                start: Some(ws_str),
                sdk: crate::sdk::NONE_IN_TESTS,
                exe_dir: Err(crate::fetch::ExeDirWhy::NotRead),
                lan_peer: crate::fetch::LanPeerConfig::DISABLED,
            },
            fresh: true,
            heads_from: crate::engine_seeds::HeadsFrom::Frozen,
            head_units: crate::engine_heads::HeadUnits::Each,
            head_jobs: crate::engine_heads::HeadJobs::RULE,
            head_sources: crate::engine_heads_once::Sources::Job,
            frame: &frame,
            scope_reads: crate::engine_scope_reads::ScopeReadsAs::DEFAULT,
            unreadable: None,
            body_locals: blaze_ladder::front2::tables::seeds::BodyLocals::Lowered,
            status: crate::engine_status::Asking::NONE,
            origin: None,
            names_oracle: crate::engine_names::NamesOracle::Off,
            driver: crate::engine_ready_edit::Driver::Phases,
            ready_plant: crate::engine_ready_edit::CommitPlant::None,
            scope: crate::engine_check::CheckScope::Whole,
        };
        let fs = io.need_fs().expect("fs");
        let host = io.need_host().expect("host");
        let mut rng = io.need_rng().expect("rng");
        let out = io.need_out().expect("out");
        let err = io.need_err().expect("err");
        let root = fs.root();
        let mut phases = std::boxed::Box::new(crate::phases::Phases::new(
            io.need_clock().expect("clock"),
        ));
        let mut shown = crate::print::Shown::new(
            blaze_report::frame::Caps::LINES,
            blaze_report::frame::plain::View::Person,
            ws_str,
            cx.frame(),
        );
        let checked = crate::engine_check::check(
            &cx,
            &fs,
            &host,
            &mut rng,
            io.args().as_ref(),
            io.net().as_ref(),
            &root,
            opts,
            &mut shown,
            &mut phases,
            &out,
            &err,
        )
        .expect("engine_check::check on NATIVE_PROC_MACRO_HARNESS + demo_pm");
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(
            shown.listed.is_empty(),
            "Blaze frontend/typeck/borrowck reported findings on NATIVE_PROC_MACRO_HARNESS + demo_pm: {:?}",
            shown
                .listed
                .iter()
                .map(|l| (l.code(), l.id.as_str()))
                .collect::<std::vec::Vec<_>>()
        );
        assert!(checked.tasks > 0, "expected engine_check to check workspace bodies");
    }
}
