// Own program x0-unwind-01 (front2; run.md piece 11, "`catch_unwind` catches"): a panic
// unwinds through code blaze built, runs the destructor of a live local once, and is caught.
//
// The Reference, "Panic": the strategy `unwind` "unwinds the stack and is potentially
// recoverable"; [panic.unwind.destruction]: "as the panic traverses Rust frames, live objects
// in those frames that implement Drop will have their drop methods called". The pinned core's
// intrinsic (library/core/src/intrinsics/mod.rs, nightly-2026-10-27): "Rust's \"try catch\"
// construct for unwinding. Invokes the function pointer `try_fn` with the data pointer `data`,
// and calls `catch_fn` if unwinding occurs while `try_fn` runs. Returns `true` if unwinding
// occurred and `catch_fn` was called; returns `false` otherwise."
//
// The program is no_core, so it raises and catches by the Itanium ABI itself: `boom` raises
// `EXC` with `_Unwind_RaiseException`; `personality` (the `eh_personality` lang item) reads
// blaze's LSDA (blaze-code eh.rs: LPStart omitted, call sites udata4, a uleb action) as the
// pinned library's dwarf/eh.rs does, with `ip = _Unwind_GetIP - 1`. `try_fn` holds a `Counted`
// of weight 40 across `boom()`, so that call has a cleanup pad that drops it (adds 40) and
// resumes; `main`'s `catch_unwind` lands in its catch pad, which calls `catch_fn` (adds 2 when
// handed `EXC` itself). Exit status 42: one drop, one catch. No drop on the unwind gives 2; a
// drop twice 82; not caught 1042 or the process ends; an unwind that never starts 1077.
//
// NEEDS (not in this program): the ladder building with `Panic::Unwind`, each placed body's
// unwind entry registered before its slot is published (blaze-launch-rt code_unwind.rs), and
// the externs `_Unwind_*` resolved from libSystem (macOS) or libgcc_s (Linux).
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}
impl Copy for u8 {}
impl Copy for usize {}
impl Copy for bool {}
impl<T> Copy for *mut T {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T>(_: &mut T) {}

#[rustc_intrinsic]
unsafe fn catch_unwind(try_fn: unsafe fn(*mut u8), data: *mut u8, catch_fn: unsafe fn(*mut u8, *mut u8)) -> bool;

/// `_Unwind_Exception` (the Itanium ABI, 2 private words on arm64), 16-aligned as libunwind's.
#[repr(C, align(16))]
struct Exc {
    class: u64,
    cleanup: usize,
    private_1: usize,
    private_2: usize,
}

unsafe extern "C" {
    fn _Unwind_RaiseException(exc: *mut Exc) -> i32;
    fn _Unwind_GetIP(ctx: *mut u8) -> usize;
    fn _Unwind_GetRegionStart(ctx: *mut u8) -> usize;
    fn _Unwind_GetLanguageSpecificData(ctx: *mut u8) -> *const u8;
    fn _Unwind_SetGR(ctx: *mut u8, reg: i32, value: usize);
    fn _Unwind_SetIP(ctx: *mut u8, ip: usize);
}

static mut COUNT: i32 = 0;
static mut EXC: Exc = Exc { class: 0x424c415a45000000, cleanup: 0, private_1: 0, private_2: 0 };

struct Counted {
    weight: i32,
}

impl Drop for Counted {
    fn drop(&mut self) {
        unsafe {
            COUNT += self.weight;
        }
    }
}

/// The byte at `p`.
unsafe fn byte(p: usize) -> usize {
    unsafe { *(p as *const u8) as usize }
}

/// The little-endian `u32` at `p`.
unsafe fn word(p: usize) -> usize {
    unsafe { byte(p) | (byte(p + 1) << 8) | (byte(p + 2) << 16) | (byte(p + 3) << 24) }
}

/// The uleb128 at `*p`, `*p` moved past it.
unsafe fn uleb(p: &mut usize) -> usize {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let b = unsafe { byte(*p) };
        *p += 1;
        value |= (b & 0x7f) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return value;
        }
    }
}

/// The personality: phase 1 says whether this frame catches (a call site whose action record's
/// type filter is positive); phase 2 installs the landing pad (cleanup or catch) with the
/// exception in x0. A call site not in the table, or a terminate action, stops the unwind.
#[lang = "eh_personality"]
unsafe extern "C" fn personality(_version: i32, actions: i32, _class: u64, exc: *mut Exc, ctx: *mut u8) -> i32 {
    unsafe {
        let lsda = _Unwind_GetLanguageSpecificData(ctx) as usize;
        if lsda == 0 {
            return 8; // _URC_CONTINUE_UNWIND: a frame with no pads
        }
        let ip = _Unwind_GetIP(ctx) - 1;
        let func = _Unwind_GetRegionStart(ctx);
        let mut p = lsda;
        if byte(p) != 0xff {
            return 3; // _URC_FATAL_PHASE1_ERROR: blaze omits LPStart
        }
        p += 1;
        let ttype = byte(p);
        p += 1;
        if ttype != 0xff {
            uleb(&mut p);
        }
        p += 1; // the call-site encoding, udata4
        let table_len = uleb(&mut p);
        let end = p + table_len;
        while p < end {
            let start = func + word(p);
            let len = word(p + 4);
            let pad = word(p + 8);
            p += 12;
            let action = uleb(&mut p);
            if ip < start {
                return 3;
            }
            if ip < start + len {
                if pad == 0 {
                    return 8;
                }
                let catches = action != 0 && byte(end + action - 1) == 0x01;
                let terminates = action != 0 && !catches;
                if terminates {
                    return 3;
                }
                if actions & 1 != 0 {
                    return if catches { 6 } else { 8 }; // _URC_HANDLER_FOUND, _URC_CONTINUE_UNWIND
                }
                _Unwind_SetGR(ctx, 0, exc as usize);
                _Unwind_SetGR(ctx, 1, 0);
                _Unwind_SetIP(ctx, func + pad);
                return 7; // _URC_INSTALL_CONTEXT
            }
        }
        3
    }
}

/// Raises `EXC`; returns only if no frame catches it.
fn boom() {
    unsafe {
        _Unwind_RaiseException(&raw mut EXC);
        COUNT += 1077;
    }
}

unsafe fn try_fn(_: *mut u8) {
    let held = Counted { weight: 40 };
    boom();
    let _ = held.weight;
}

unsafe fn catch_fn(_: *mut u8, exc: *mut u8) {
    unsafe {
        if exc as usize == &raw mut EXC as usize {
            COUNT += 2;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe {
        let caught = catch_unwind(try_fn, 0 as *mut u8, catch_fn);
        if !caught {
            COUNT += 1000;
        }
        COUNT
    }
}
