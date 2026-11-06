//! The C ABI the page calls, over the module's linear memory; no wasm-bindgen.
//!
//! The page writes input into memory it took with [`explore_alloc`], calls [`explore_feed`] or
//! [`explore_view`], and reads the result where [`explore_out_ptr`]/[`explore_out_len`] (or,
//! for a non-zero status, [`explore_error_ptr`]/[`explore_error_len`], UTF-8) say. That memory
//! is the module's until the next call that writes it: copy it out before calling again.
//!
//! | export | signature (wasm) | what |
//! |---|---|---|
//! | `explore_abi` | `() -> i32` | [`ABI`]: the page refuses a core of another |
//! | `explore_alloc` | `(len: i32) -> i32` | `len` bytes for the page to write; 0 when they do not fit |
//! | `explore_free` | `(ptr: i32, len: i32)` | gives back what `explore_alloc(len)` gave |
//! | `explore_new` | `() -> i32` | a session's handle, never 0; 0 when none can be made |
//! | `explore_drop` | `(handle: i32) -> i32` | ends a session |
//! | `explore_feed` | `(handle: i32, kind: i32, ptr: i32, len: i32) -> i32` | bytes from the daemon ([`Kind`]) |
//! | `explore_view` | `(handle: i32, which: i32, now_ns: i64) -> i32` | a view ([`Which`]) as CBOR into out |
//! | `explore_out_ptr`, `explore_out_len` | `() -> i32` | the last view's CBOR |
//! | `explore_error_ptr`, `explore_error_len` | `() -> i32` | the last failure's text |
//!
//! Every call returns a status: [`OK`], [`ERROR`] (the bytes or the state refused, the error
//! naming the message and the field), [`NO_HANDLE`], [`BAD_ARG`] (an unknown kind or view, a
//! null pointer with a length). Every non-zero status sets the error text; [`OK`] clears it.
//! `now_ns` is the daemon's clock (hello's `clock`) as the page reckons it: hello's `now_ns`
//! plus the time the page has measured since.
//!
//! A panic traps the module (wasm32-unknown-unknown aborts): the page then makes it anew. The
//! core is written not to panic on any bytes; a trap is a bug in it.

#![allow(unsafe_code)]

use std::{alloc::Layout, cell::RefCell, collections::BTreeMap, ptr::NonNull};

use arrow_ipc::reader::StreamDecoder;

use crate::{
    state::Session,
    view::{self, Which},
};

/// This ABI's version: the exports, their signatures and meaning, and the views' shapes.
pub const ABI: u32 = 1;

/// [`ABI`] in the module's custom section `ix.explore.abi`, four bytes little-endian, so the
/// program serving the core (`ix_prof::explore::wasm::SECTION`) says it in `hello` without
/// running the module. Only the wasm build has it: the section's name is no Mach-O or ELF one.
#[cfg(target_arch = "wasm32")]
#[used]
#[unsafe(link_section = "ix.explore.abi")]
static ABI_SECTION: [u8; 4] = ABI.to_le_bytes();

pub const OK: i32 = 0;
pub const ERROR: i32 = 1;
pub const NO_HANDLE: i32 = 2;
pub const BAD_ARG: i32 = 3;

/// What [`explore_feed`]'s bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    /// The answer to the session's first request: hello, whole.
    Hello = 1,
    /// One live frame: the bytes after its `u32` length.
    LiveFrame = 2,
    /// Bytes of the live stream as they arrive, split anywhere: the core cuts the frames.
    LiveStream = 3,
    /// An answer to another request, whole.
    Answer = 4,
}

impl Kind {
    pub fn from_u32(kind: u32) -> Option<Kind> {
        Some(match kind {
            1 => Kind::Hello,
            2 => Kind::LiveFrame,
            3 => Kind::LiveStream,
            4 => Kind::Answer,
            _ => return None,
        })
    }
}

/// The module's state: the sessions, each reading IPC through arrow-ipc's decoder (the driver's
/// choice, [`crate::ipc`]), and the buffers the page reads.
struct Core {
    sessions: BTreeMap<u32, Session<StreamDecoder>>,
    /// The next handle; 0 is never one.
    next: u32,
    out: Vec<u8>,
    error: Vec<u8>,
}

impl Core {
    /// `result` as a status, its error text kept for the page.
    fn status(&mut self, result: Result<(), (i32, String)>) -> i32 {
        self.error.clear();
        match result {
            Ok(()) => OK,
            Err((status, why)) => {
                // alloc: the error's text, in the buffer kept for the page
                self.error.extend_from_slice(why.as_bytes());
                status
            }
        }
    }
}

// One a thread, and the module has one thread: the page's.
thread_local! {
    static CORE: RefCell<Core> = const {
        RefCell::new(Core { sessions: BTreeMap::new(), next: 1, out: Vec::new(), error: Vec::new() })
    };
}

/// `f` on the module's state. No export calls another while inside, so the borrow is never
/// taken twice.
fn with_core<R>(f: impl FnOnce(&mut Core) -> R) -> R {
    CORE.with_borrow_mut(f)
}

#[unsafe(no_mangle)]
pub extern "C" fn explore_abi() -> u32 {
    ABI
}

/// `len` bytes for the page to write, given back with [`explore_free`] and the same `len`;
/// null when they do not fit, a dangling (never read) pointer for 0.
#[unsafe(no_mangle)]
pub extern "C" fn explore_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return NonNull::<u8>::dangling().as_ptr();
    }
    let Ok(layout) = Layout::array::<u8>(len) else { return std::ptr::null_mut() };
    // SAFETY: `layout` is not zero-sized (`len` is not 0). A null result is handed on: the
    // page reads it as "does not fit".
    unsafe { std::alloc::alloc(layout) }
}

/// Gives back what [`explore_alloc`] gave.
///
/// # Safety
///
/// `ptr` came from `explore_alloc(len)` with this `len`, and is not yet given back.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explore_free(ptr: *mut u8, len: usize) {
    if len == 0 || ptr.is_null() {
        return;
    }
    let Ok(layout) = Layout::array::<u8>(len) else { return };
    // SAFETY: the caller's promise: `ptr` is `explore_alloc(len)`'s, which allocated it with
    // this very layout, and is freed once.
    unsafe { std::alloc::dealloc(ptr, layout) }
}

/// A new session's handle, never 0; 0 (the error text says why) when none can be made.
#[unsafe(no_mangle)]
pub extern "C" fn explore_new() -> u32 {
    with_core(|core| {
        let handle = core.next;
        let Some(next) = handle.checked_add(1) else {
            // alloc: an error's text
            core.status(Err((ERROR, "explore_new: every handle up to 2^32 - 1 was given".to_owned())));
            return 0;
        };
        core.next = next;
        // alloc: once a session
        core.sessions.insert(handle, Session::new());
        core.status(Ok(()));
        handle
    })
}

/// Ends the session `handle`.
#[unsafe(no_mangle)]
pub extern "C" fn explore_drop(handle: u32) -> i32 {
    with_core(|core| {
        let result = match core.sessions.remove(&handle) {
            Some(_) => Ok(()),
            // alloc: an error's text
            None => Err((NO_HANDLE, format!("explore_drop: no session has handle {handle}"))),
        };
        core.status(result)
    })
}

/// Bytes from the daemon into session `handle`, `kind` saying which ([`Kind`]). The bytes are
/// read during the call only: the page frees or reuses them after.
///
/// # Safety
///
/// When `len` is not 0, `ptr` is valid for reads of `len` bytes and nothing writes them during
/// the call (memory from [`explore_alloc`], or any other the page owns in this module).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn explore_feed(handle: u32, kind: u32, ptr: *const u8, len: usize) -> i32 {
    let bytes: &[u8] = if len == 0 {
        &[]
    } else if ptr.is_null() {
        return with_core(|core| {
            // alloc: an error's text
            core.status(Err((BAD_ARG, format!("explore_feed: a null pointer with {len} bytes"))))
        });
    } else {
        // SAFETY: the caller's promise: `ptr` is not null (checked), readable for `len` bytes,
        // and not written during this call; the slice does not outlive it.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    };
    with_core(|core| {
        let result = match (core.sessions.get_mut(&handle), Kind::from_u32(kind)) {
            // alloc: an error's text
            (None, _) => Err((NO_HANDLE, format!("explore_feed: no session has handle {handle}"))),
            (Some(_), None) => Err((
                BAD_ARG,
                // alloc: an error's text
                format!("explore_feed: kind {kind} is none of 1 hello, 2 live frame, 3 live stream, 4 answer"),
            )),
            (Some(session), Some(kind)) => match kind {
                Kind::Hello => session.feed_hello(bytes),
                Kind::LiveFrame => session.feed_live_frame(bytes),
                Kind::LiveStream => session.feed_live_stream(bytes),
                Kind::Answer => session.feed_answer(bytes),
            }
            // alloc: an error's text
            .map_err(|why| (ERROR, why.to_string())),
        };
        core.status(result)
    })
}

/// View `which` ([`Which`]) of session `handle` at `now_ns`, as one CBOR item into out
/// ([`explore_out_ptr`], [`explore_out_len`]); out is empty after a failure.
#[unsafe(no_mangle)]
pub extern "C" fn explore_view(handle: u32, which: u32, now_ns: i64) -> i32 {
    with_core(|core| {
        let Core { sessions, out, .. } = &mut *core;
        out.clear();
        let result = match (sessions.get_mut(&handle), Which::from_u32(which)) {
            // alloc: an error's text
            (None, _) => Err((NO_HANDLE, format!("explore_view: no session has handle {handle}"))),
            (Some(_), None) => Err((
                BAD_ARG,
                // alloc: an error's text
                format!(
                    "explore_view: view {which} is none of 1 running now, 2 recent, 3 counters, 4 session"
                ),
            )),
            (Some(session), Some(which)) => view::render(session, which, now_ns, out)
                // alloc: an error's text
                .map_err(|why| (ERROR, why.to_string())),
        };
        if result.is_err() {
            out.clear();
        }
        core.status(result)
    })
}

/// Where the last view's CBOR starts; valid until the next call to [`explore_view`].
#[unsafe(no_mangle)]
pub extern "C" fn explore_out_ptr() -> *const u8 {
    with_core(|core| core.out.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn explore_out_len() -> usize {
    with_core(|core| core.out.len())
}

/// Where the last failure's text (UTF-8) starts; valid until the next call.
#[unsafe(no_mangle)]
pub extern "C" fn explore_error_ptr() -> *const u8 {
    with_core(|core| core.error.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn explore_error_len() -> usize {
    with_core(|core| core.error.len())
}

/// The host's view of out and the error text, for tests (the page reads memory instead).
#[cfg(test)]
pub(crate) fn out_and_error() -> (Vec<u8>, String) {
    with_core(|core| {
        // clone: a test's copy
        (core.out.clone(), String::from_utf8_lossy(&core.error).into_owned())
    })
}
