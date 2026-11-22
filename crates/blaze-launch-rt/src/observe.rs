//! Registers a stub may write: AArch64 x16 and x17 only (flags untouched); x86-64 r10 and r11, and the flags.
//!
//! The observe stubs as machine code, and the per-thread mark bitmap they write.
//!
//! A slot's word can point for a while at one of three small stubs instead of at the body
//! (`blaze_launch::observe`). None is compiled into the body: a body's bytes are the same
//! whatever is observed, so they stay a shareable record. Each stub ends in a jump to the
//! address it reads from the function's **body word**, a `.quad` at a fixed offset in the stub
//! that the control thread overwrites on a publish (one aligned store a thread reads as data,
//! so no instruction-cache flush), so the stub is written once and never re-emitted when the
//! body changes.
//!
//! A stub runs between a call and the callee's first instruction, so every argument register
//! (x0 to x7, x8 the indirect-result address, v0 to v7), the stack pointer and the return
//! address must reach the body as the caller left them. AAPCS64 gives a veneer x16 and x17
//! (IP0, IP1) and nothing else; the count and mark stubs write those two alone, and the hook
//! trampoline saves and restores everything else it or the Rust routine it calls may touch.
//!
//! **A stub never stores a slot word or a body word**: the control thread is the only writer
//! of either (`blaze-run`'s `Slots`). A stub writes only the counter table and the mark
//! bitmap, which are ordinary data a program thread stores to, so they live in the mapping's
//! data region (`sys::Code::data_base`), never in the code area. The stub code is in the code
//! area, written by the control thread like any body.
//!
//! The three kinds:
//!
//! - [`count`]: counts down a per-function 32-bit counter in the data table (not atomic: a lost
//!   count only cools a function a little). The control thread stores the threshold as the
//!   starting count; a call that finds it at zero sets the **queued** word beside it instead,
//!   so the stub needs no compare register and touches no flags. The control thread drains
//!   the queued words on its clock, raises `Heat::reached(slot)` (idempotent: a function is
//!   queued again on every call until it is made plain), and does `plain(slot)` and the
//!   optimizing compile; the stub itself stores no word.
//! - [`mark`]: sets one byte in a per-thread bitmap indexed by the function id, then jumps on.
//!   The bitmap's base is a pointer at a fixed offset from the thread pointer. On macOS it is
//!   a pthread key's value: `TPIDRRO_EL0` (its low three bits masked off, as libsystem's
//!   `_os_tsd_get_base` does) is the thread's TSD array, and key `k`'s value is at `8 * k`.
//!   On Linux it is an initial-exec TLS word of the launcher at its positive offset from
//!   `TPIDR_EL0` (AArch64 TLS variant 1). On x86-64 it is `fs:` at the same kind of offset.
//! - [`hook_stub`]: a tail branch to [`hook_entry`], which calls a runtime routine with the
//!   function id and the return address, then jumps on (for the monitor's count, break and
//!   allocation sampling). A hook entry out of branch reach goes through a [`veneer`].
//!
//! Some byte sequences are adapted from makepad/relax (MIT): the counting and entry-mark
//! stubs of `libs/relax/backend/src/arm64.rs` and `.../x64.rs`.

#[cfg(target_arch = "aarch64")]
use crate::arch::OutOfReach;

/// One entry of the counter table, 16 bytes, little-endian and deterministic; the table is in
/// the data region, one entry a counted function. `left` counts down from `threshold`; a call
/// that finds it at zero sets `queued` to 1 (so the function is queued on the call after
/// `threshold` counted ones). The control thread re-arms an entry by storing `threshold` in
/// `left` and 0 in `queued`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct Counter {
    pub left: u32,
    pub threshold: u32,
    pub queued: u32,
    _pad: u32,
}

impl Counter {
    /// A fresh counter that queues after `threshold` calls.
    pub fn new(threshold: u32) -> Counter {
        Counter { left: threshold, threshold, queued: 0, _pad: 0 }
    }
}

/// The offset of [`Counter::queued`] in a [`Counter`], which the count stub stores to.
const QUEUED: u32 = 8;

/// Which kind of stub a slot's word names: the control thread stores a body at a kind's body
/// word offset ([`body_offset`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Count,
    Mark,
    Hook,
    Span,
    Alloc,
}

/// The bytes of a count stub (AArch64), including its trailing body-word `.quad`.
#[cfg(target_arch = "aarch64")]
pub const COUNT_STUB: usize = 64;
/// The bytes of a mark stub (AArch64), including its trailing body-word `.quad`.
#[cfg(target_arch = "aarch64")]
pub const MARK_STUB: usize = 48;
/// The bytes of a hook stub (AArch64): `adr x16, .; b hook_entry; .quad slot; .quad body`.
#[cfg(target_arch = "aarch64")]
pub const HOOK_STUB: usize = 24;
/// The bytes of a span stub (AArch64): `adr x16, .; b span_entry; .quad slot; .quad body`.
#[cfg(target_arch = "aarch64")]
pub const SPAN_STUB: usize = 24;
/// The bytes of a Rust global-allocator stub (AArch64): `adr x16, .; b alloc_entry; .quad op; .quad body`.
#[cfg(target_arch = "aarch64")]
pub const ALLOC_STUB: usize = 24;
/// The bytes of a veneer (AArch64): `ldr x17, .+8; br x17; .quad target`.
#[cfg(target_arch = "aarch64")]
pub const VENEER: usize = 16;

/// The largest function id a mark stub takes, plus one: two 12-bit immediates into x16.
#[cfg(target_arch = "aarch64")]
pub const MARK_SLOTS: u32 = 1 << 24;

/// The byte offset of the body-word `.quad` in a stub of each kind. The control thread stores
/// the plain body address here on a publish (8-aligned: one atomic store a thread reads via a
/// data load, needing no instruction-cache flush).
#[cfg(target_arch = "aarch64")]
pub const fn body_offset(kind: Kind) -> usize {
    match kind {
        Kind::Count => 56,
        Kind::Mark => 40,
        Kind::Hook | Kind::Span | Kind::Alloc => 16,
    }
}

#[cfg(target_arch = "aarch64")]
mod a64 {
    pub(super) const fn movz(rd: u32, imm16: u16, shift: u32) -> u32 {
        0xD280_0000 | (shift << 21) | ((imm16 as u32) << 5) | rd
    }
    pub(super) const fn movk(rd: u32, imm16: u16, shift: u32) -> u32 {
        0xF280_0000 | (shift << 21) | ((imm16 as u32) << 5) | rd
    }
    /// `movz w<rd>, #imm16`.
    pub(super) const fn movz_w(rd: u32, imm16: u16) -> u32 {
        0x5280_0000 | ((imm16 as u32) << 5) | rd
    }
    pub(super) const fn mov_abs(rd: u32, value: u64) -> [u32; 4] {
        [
            movz(rd, value as u16, 0),
            movk(rd, (value >> 16) as u16, 1),
            movk(rd, (value >> 32) as u16, 2),
            movk(rd, (value >> 48) as u16, 3),
        ]
    }
    /// `ldr w<rt>, [x<rn>, #<off>]`: `off` a multiple of 4 below 16384.
    pub(super) const fn ldr_w(rt: u32, rn: u32, off: u32) -> u32 {
        0xB940_0000 | ((off / 4) << 10) | (rn << 5) | rt
    }
    /// `str w<rt>, [x<rn>, #<off>]`: `off` a multiple of 4 below 16384.
    pub(super) const fn str_w(rt: u32, rn: u32, off: u32) -> u32 {
        0xB900_0000 | ((off / 4) << 10) | (rn << 5) | rt
    }
    /// `ldr x<rt>, [x<rn>, #<off>]`: `off` a multiple of 8 below 32768.
    pub(super) const fn ldr_x(rt: u32, rn: u32, off: u32) -> u32 {
        0xF940_0000 | ((off / 8) << 10) | (rn << 5) | rt
    }
    /// `strb w<rt>, [x<rn>]`.
    pub(super) const fn strb(rt: u32, rn: u32) -> u32 {
        0x3900_0000 | (rn << 5) | rt
    }
    /// `add x<rd>, x<rn>, #<imm12>` (`shift12`: `imm12` shifted left 12). Sets no flags.
    pub(super) const fn add_imm(rd: u32, rn: u32, imm12: u32, shift12: bool) -> u32 {
        0x9100_0000 | ((shift12 as u32) << 22) | (imm12 << 10) | (rn << 5) | rd
    }
    /// `sub w<rd>, w<rn>, #<imm12>`. Sets no flags (not `subs`).
    pub(super) const fn sub_imm_w(rd: u32, rn: u32, imm12: u32) -> u32 {
        0x5100_0000 | (imm12 << 10) | (rn << 5) | rd
    }
    /// `and x<rd>, x<rn>, #~7` (logical immediate N=1, immr=61, imms=60). Sets no flags.
    pub(super) const fn and_not7(rd: u32, rn: u32) -> u32 {
        0x9240_0000 | (61 << 16) | (60 << 10) | (rn << 5) | rd
    }
    /// `cbz w<rt>, .+delta`: `delta` a multiple of 4 within 1 MiB. Reads no flags.
    pub(super) const fn cbz_w(rt: u32, delta: i64) -> u32 {
        0x3400_0000 | ((((delta >> 2) as u32) & 0x7_ffff) << 5) | rt
    }
    /// `ldr x<rt>, .+delta` (a literal load).
    pub(super) const fn ldr_lit(rt: u32, delta: i64) -> u32 {
        0x5800_0000 | ((((delta >> 2) as u32) & 0x7_ffff) << 5) | rt
    }
    pub(super) const fn br(rn: u32) -> u32 {
        0xD61F_0000 | (rn << 5)
    }
    pub(super) const fn b(delta: i64) -> u32 {
        0x1400_0000 | (((delta >> 2) as u32) & 0x03ff_ffff)
    }
    pub(super) const fn adr_here(rd: u32) -> u32 {
        0x1000_0000 | rd
    }
    /// `udf #0`: the padding after a stub's last branch, which nothing reaches.
    pub(super) const UDF: u32 = 0;
    /// `mrs x<rt>, <sysreg>`: `sysreg` is the instruction with Rt = 0 ([`TPIDRRO_EL0`],
    /// [`TPIDR_EL0`]), so the register is never left out of the word.
    pub(super) const fn mrs(rt: u32, sysreg: u32) -> u32 {
        sysreg | rt
    }
    /// `mrs x0, TPIDRRO_EL0` (op0 3, op1 3, CRn 13, CRm 0, op2 3): Rt is or-ed in by [`mrs`].
    #[cfg_attr(target_os = "linux", allow(dead_code, reason = "macOS's thread register"))]
    pub(super) const TPIDRRO_EL0: u32 = 0xd53b_d060;
    /// `mrs x0, TPIDR_EL0` (op2 2): Rt is or-ed in by [`mrs`].
    #[cfg_attr(target_os = "macos", allow(dead_code, reason = "Linux's thread register"))]
    pub(super) const TPIDR_EL0: u32 = 0xd53b_d040;
}

#[cfg(target_arch = "aarch64")]
fn emit(code: &[u32], into: &mut [u8]) {
    for (i, word) in code.iter().enumerate() {
        into[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
}

/// A count stub at `at`, counting down the 16-byte [`Counter`] at `counter` (in the data
/// region). `body` is the initial plain body the control thread also stores at the body word
/// ([`body_offset`]`(Kind::Count)`). Writes x16 and x17 only, and no flags.
///
/// adapted from makepad/relax (MIT), libs/relax/backend/src/arm64.rs (`count_stub`).
#[cfg(target_arch = "aarch64")]
pub fn count(at: u64, counter: u64, body: u64) -> Result<[u8; COUNT_STUB], OutOfReach> {
    use a64::*;
    if counter % 16 != 0 {
        return Err(OutOfReach { what: "a count stub's counter is not 16-aligned", from: at, to: counter });
    }
    // Offsets (body `.quad` at 56):
    //  0 movz/movk x16, counter    (4 words)
    // 16 ldr  w17, [x16]           ; left
    // 20 cbz  w17, .+20            ; at zero: to 40
    // 24 sub  w17, w17, #1
    // 28 str  w17, [x16]
    // 32 ldr  x16, .+24            ; the body word at 56
    // 36 br   x16
    // 40 movz w17, #1              ; at_zero
    // 44 str  w17, [x16, #8]       ; queued
    // 48 ldr  x16, .+8             ; the body word at 56
    // 52 br   x16
    // 56 .quad body
    let ci = mov_abs(16, counter);
    let code: [u32; 14] = [
        ci[0],
        ci[1],
        ci[2],
        ci[3],
        ldr_w(17, 16, 0),
        cbz_w(17, 20),
        sub_imm_w(17, 17, 1),
        str_w(17, 16, 0),
        ldr_lit(16, 24),
        br(16),
        movz_w(17, 1),
        str_w(17, 16, QUEUED),
        ldr_lit(16, 8),
        br(16),
    ];
    let mut bytes = [0u8; COUNT_STUB];
    emit(&code, &mut bytes);
    bytes[56..64].copy_from_slice(&body.to_le_bytes());
    Ok(bytes)
}

/// A mark stub at `at` for the function `slot`: reads this thread's bitmap base from the
/// thread pointer at `tp_off`, sets the byte at `slot`, then loads the body word and jumps.
/// `body` is the initial body the control thread also stores at the body word. Writes x16
/// and x17 only, and no flags; a `slot` of [`MARK_SLOTS`] or more is refused.
///
/// adapted from makepad/relax (MIT), libs/relax/backend/src/arm64.rs (`entry_thunk`).
#[cfg(target_arch = "aarch64")]
pub fn mark(at: u64, tp_off: u32, slot: u32, body: u64) -> Result<[u8; MARK_STUB], OutOfReach> {
    use a64::*;

    #[cfg(target_os = "macos")]
    let tp = mrs(16, TPIDRRO_EL0);
    #[cfg(target_os = "linux")]
    let tp = mrs(16, TPIDR_EL0);
    if tp_off % 8 != 0 || tp_off >= 1 << 15 {
        return Err(OutOfReach { what: "a mark stub's thread-pointer offset", from: at, to: u64::from(tp_off) });
    }
    if slot >= MARK_SLOTS {
        return Err(OutOfReach { what: "a mark stub's function id is 2^24 or more", from: at, to: u64::from(slot) });
    }
    // Offsets (body `.quad` at 40):
    //  0 mrs  x16, tp
    //  4 and  x16, x16, #~7        ; older macOS kept the CPU number in the low bits; harmless
    //                              ; where it is clean (macOS 27: the TSD base, measured 2026-11-15)
    //  8 ldr  x16, [x16, #tp_off]  ; this thread's bitmap base
    // 12 add  x16, x16, #slot_lo
    // 16 add  x16, x16, #slot_hi, lsl 12
    // 20 movz w17, #1
    // 24 strb w17, [x16]
    // 28 ldr  x16, .+12            ; the body word at 40
    // 32 br   x16
    // 36 udf  #0
    // 40 .quad body
    let code: [u32; 10] = [
        tp,
        and_not7(16, 16),
        ldr_x(16, 16, tp_off),
        add_imm(16, 16, slot & 0xfff, false),
        add_imm(16, 16, (slot >> 12) & 0xfff, true),
        movz_w(17, 1),
        strb(17, 16),
        ldr_lit(16, 12),
        br(16),
        UDF,
    ];
    let mut bytes = [0u8; MARK_STUB];
    emit(&code, &mut bytes);
    bytes[40..48].copy_from_slice(&body.to_le_bytes());
    Ok(bytes)
}

/// A hook stub at `at`: `adr x16, .; b entry; .quad 0; .quad body`. The branch is near (±128 MiB):
/// `entry` is [`hook_entry`] or a [`veneer`] to it in the same mapping.
#[cfg(target_arch = "aarch64")]
pub fn hook_stub(at: u64, entry: u64, body: u64) -> Result<[u8; HOOK_STUB], OutOfReach> {
    hook_stub_slot(at, entry, 0, body)
}

/// A hook stub at `at` carrying its `slot` id at offset 8 so [`hook_resolve`] identifies the
/// slot in one load with no table lookup: `adr x16, .; b entry; .quad slot; .quad body`.
#[cfg(target_arch = "aarch64")]
pub fn hook_stub_slot(at: u64, entry: u64, slot: u32, body: u64) -> Result<[u8; HOOK_STUB], OutOfReach> {
    use a64::*;

    let delta = entry.wrapping_sub(at + 4) as i64;
    if delta % 4 != 0 || !(-(1 << 27)..(1 << 27)).contains(&delta) {
        return Err(OutOfReach { what: "a hook stub's branch to the hook entry", from: at + 4, to: entry });
    }
    let code: [u32; 2] = [adr_here(16), b(delta)];
    let mut bytes = [0u8; HOOK_STUB];
    emit(&code, &mut bytes);
    bytes[8..16].copy_from_slice(&u64::from(slot).to_le_bytes());
    bytes[16..24].copy_from_slice(&body.to_le_bytes());
    Ok(bytes)
}

/// An entry+exit span stub at `at` carrying its `slot` id at offset 8:
/// `adr x16, .; b entry; .quad slot; .quad body`, where `entry` is [`span_entry`] or a veneer.
#[cfg(target_arch = "aarch64")]
pub fn span_stub(at: u64, entry: u64, slot: u32, body: u64) -> Result<[u8; SPAN_STUB], OutOfReach> {
    hook_stub_slot(at, entry, slot, body)
}

/// A Rust global-allocator slot stub at `at` carrying its [`blaze_launch::telemetry::AllocHookKind`]
/// at offset 8: `adr x16, .; b entry; .quad kind; .quad body`, where `entry` is [`alloc_entry`] or
/// a veneer. Wraps `__rust_alloc`, `__rust_dealloc`, `__rust_realloc`, or `__rust_alloc_zeroed`
/// (and their `__rdl_*` defaults) so every Rust allocator — `System`, `jemalloc`, `mimalloc`, or
/// custom `#[global_allocator]` — emits live telemetry with zero code changes.
#[cfg(target_arch = "aarch64")]
pub fn alloc_stub(
    at: u64,
    entry: u64,
    kind: blaze_launch::telemetry::AllocHookKind,
    body: u64,
) -> Result<[u8; ALLOC_STUB], OutOfReach> {
    hook_stub_slot(at, entry, kind as u32, body)
}

/// Naked AArch64 entry for [`alloc_stub`]: preserves the allocator's 2..4 scalar arguments in
/// `x0..x3`, establishes a standard frame record `(x29, x30)` so frame-pointer stack walks attribute
/// directly to the caller, passes the stub address (`x16`) in `x4`, and returns `x0` unchanged.
///
/// # Safety
///
/// Reached only by a branch from [`alloc_stub`] (directly or via [`veneer`]) with `x16 = stub`.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn alloc_entry() {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        "mov x4, x16",
        "bl {dispatch}",
        "ldp x29, x30, [sp], #16",
        "ret",
        dispatch = sym alloc_dispatch,
    );
}

#[cfg(target_arch = "aarch64")]
extern "C" fn alloc_dispatch(a0: usize, a1: usize, a2: usize, a3: usize, stub: usize) -> usize {
    use blaze_launch::telemetry::{AllocHookKind, AllocOp, Event};
    use core::sync::atomic::{AtomicU64, Ordering};

    // SAFETY: `stub` is an 8-aligned live `ALLOC_STUB` with `kind` at +8 and atomic `body` at +16.
    let kind_raw = unsafe { *((stub + 8) as *const u64) } as u8;
    let body = unsafe { &*((stub + 16) as *const AtomicU64) }.load(Ordering::Acquire) as usize;
    let Some(kind) = AllocHookKind::from_raw(kind_raw) else {
        return 0;
    };

    // Shared re-entrancy guard: if `body` is `std::alloc::System` calling `ObservedBind::malloc`,
    // `traced_malloc` sees `try_enter_alloc_hook() == None` and skips double-counting!
    let guard = crate::telemetry::try_enter_alloc_hook();

    match kind {
        AllocHookKind::Alloc | AllocHookKind::AllocZeroed => {
            // SAFETY: `__rust_alloc(size, align) -> *mut u8` / `__rust_alloc_zeroed(size, align)`.
            let f: unsafe extern "C" fn(usize, usize) -> *mut u8 = unsafe { core::mem::transmute(body) };
            let ptr = unsafe { f(a0, a1) };
            if guard.is_some() && crate::telemetry::should_sample_alloc(a0) {
                let frames = crate::telemetry::capture_caller_frames(2);
                let _ = crate::telemetry::emit(Event::alloc(
                    crate::telemetry::current_thread_id(),
                    kind.alloc_op(),
                    crate::telemetry::now_ns(),
                    ptr as usize as u64,
                    a0 as u64,
                    frames,
                ));
            }
            ptr as usize
        }
        AllocHookKind::Dealloc => {
            // SAFETY: `__rust_dealloc(ptr, size, align)`.
            let f: unsafe extern "C" fn(*mut u8, usize, usize) = unsafe { core::mem::transmute(body) };
            unsafe { f(a0 as *mut u8, a1, a2) };
            if guard.is_some() && crate::telemetry::alloc_sample_bytes() == 0 && a0 != 0 {
                let frames = crate::telemetry::capture_caller_frames(2);
                let _ = crate::telemetry::emit(Event::alloc(
                    crate::telemetry::current_thread_id(),
                    AllocOp::RustDealloc,
                    crate::telemetry::now_ns(),
                    a0 as u64,
                    a1 as u64,
                    frames,
                ));
            }
            0
        }
        AllocHookKind::Realloc => {
            // SAFETY: `__rust_realloc(ptr, old_size, align, new_size) -> *mut u8`.
            let f: unsafe extern "C" fn(*mut u8, usize, usize, usize) -> *mut u8 =
                unsafe { core::mem::transmute(body) };
            let ptr = unsafe { f(a0 as *mut u8, a1, a2, a3) };
            if guard.is_some() && crate::telemetry::should_sample_alloc(a3) {
                let frames = crate::telemetry::capture_caller_frames(2);
                let _ = crate::telemetry::emit(Event::alloc(
                    crate::telemetry::current_thread_id(),
                    AllocOp::RustRealloc,
                    crate::telemetry::now_ns(),
                    ptr as usize as u64,
                    a3 as u64,
                    frames,
                ));
            }
            ptr as usize
        }
    }
}

/// A veneer: `ldr x17, .+8; br x17; .quad target`, for a branch whose target is beyond
/// ±128 MiB. It writes x17 alone, so x16 (a hook stub's own address) passes through it.
/// The island of veneers per code-area span that the linker places is built on this.
#[cfg(target_arch = "aarch64")]
pub fn veneer(target: u64) -> [u8; VENEER] {
    use a64::*;
    let mut bytes = [0u8; VENEER];
    emit(&[ldr_lit(17, 8), br(17)], &mut bytes);
    bytes[8..16].copy_from_slice(&target.to_le_bytes());
    bytes
}

/// Where a hook stub's branch arrives: x16 is the stub's own address. Saves x0 to x15, x18
/// and v0 to v7 (whole q registers), calls [`hook_resolve`] with the stub address and the
/// caller's return address, restores them, then jumps to the body read from the stub's body
/// word (x16 + 16). The frame is 288 bytes, so sp stays 16-aligned for the call; x29 and x30
/// are saved and restored by the frame record. x9 to x15 are dead at a callee's entry by
/// AAPCS64 and x18 is the platform register: they are kept anyway, so a body sees every
/// register but x16 and x17 as its caller left it.
///
/// # Safety
///
/// Reached only by a branch from a hook stub ([`hook_stub`]), directly or through a
/// [`veneer`]: x16 is the stub's address and x30 the caller's return address. It does not
/// follow the C convention on entry or exit.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn hook_entry() {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-288]!",
        "mov x29, sp",
        "stp x0, x1, [sp, #16]",
        "stp x2, x3, [sp, #32]",
        "stp x4, x5, [sp, #48]",
        "stp x6, x7, [sp, #64]",
        "stp x8, x9, [sp, #80]",
        "stp x10, x11, [sp, #96]",
        "stp x12, x13, [sp, #112]",
        "stp x14, x15, [sp, #128]",
        // x16 (the stub) is not kept across a call, so it goes on the stack beside x18.
        "stp x16, x18, [sp, #144]",
        "stp q0, q1, [sp, #160]",
        "stp q2, q3, [sp, #192]",
        "stp q4, q5, [sp, #224]",
        "stp q6, q7, [sp, #256]",
        "mov x0, x16", // the stub address: hook_resolve derives the slot from it
        "mov x1, x30", // the caller's return address
        "bl {resolve}",
        "ldp q6, q7, [sp, #256]",
        "ldp q4, q5, [sp, #224]",
        "ldp q2, q3, [sp, #192]",
        "ldp q0, q1, [sp, #160]",
        "ldp x16, x18, [sp, #144]",
        "ldp x14, x15, [sp, #128]",
        "ldp x12, x13, [sp, #112]",
        "ldp x10, x11, [sp, #96]",
        "ldp x8, x9, [sp, #80]",
        "ldp x6, x7, [sp, #64]",
        "ldp x4, x5, [sp, #48]",
        "ldp x2, x3, [sp, #32]",
        "ldp x0, x1, [sp, #16]",
        "ldp x29, x30, [sp], #288",
        // Jump to the body read from the stub's body word (offset 16).
        "ldr x16, [x16, #16]",
        "br x16",
        resolve = sym hook_resolve,
    )
}

/// How many times [`hook_resolve`] ran, for the tests.
#[cfg(all(test, target_arch = "aarch64"))]
static HOOKED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// What a hook stub calls: `stub` is the stub's address, `retaddr` the caller's return address.
/// Reads the embedded slot id from `[stub + 8]` and pushes a [`crate::telemetry::EventKind::HookHit`]
/// into the active shared-memory telemetry ring. Stores no slot word.
#[cfg(target_arch = "aarch64")]
extern "C" fn hook_resolve(stub: usize, retaddr: usize) {
    // SAFETY: `stub` is a live 24-byte `hook_stub_slot` whose bytes `8..16` hold `u64::from(slot)`.
    let slot = unsafe { ((stub + 8) as *const u64).read() } as u32;
    let tid = crate::telemetry::current_thread_id();
    let ts = crate::telemetry::now_ns();
    let _ = crate::telemetry::emit(crate::telemetry::Event::hook_hit(
        tid,
        blaze_launch::Slot(slot),
        ts,
        retaddr as u64,
    ));
    #[cfg(test)]
    HOOKED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Entry trampoline for [`span_stub`]: saves all argument registers (`x0..x18`, `q0..q7`, `x29`,
/// `x30`), calls [`span_enter`] with `(stub, real_lr, caller_sp)`. If [`span_enter`] returns a
/// non-zero exit trampoline address ([`span_exit`]), replaces the saved `x30` slot before
/// restoring `sp` to the exact `caller_sp` (keeping stack-passed arguments untouched) and
/// tail-branching to `[x16 + 16]` (the slot's body word).
///
/// # Safety
///
/// Reached only from a [`span_stub`] (`x16 = stub`, `x30 = caller's return address`).
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn span_entry() {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-288]!",
        "mov x29, sp",
        "stp x0, x1, [sp, #16]",
        "stp x2, x3, [sp, #32]",
        "stp x4, x5, [sp, #48]",
        "stp x6, x7, [sp, #64]",
        "stp x8, x9, [sp, #80]",
        "stp x10, x11, [sp, #96]",
        "stp x12, x13, [sp, #112]",
        "stp x14, x15, [sp, #128]",
        "stp x16, x18, [sp, #144]",
        "stp q0, q1, [sp, #160]",
        "stp q2, q3, [sp, #192]",
        "stp q4, q5, [sp, #224]",
        "stp q6, q7, [sp, #256]",
        "mov x0, x16",
        "mov x1, x30",
        "add x2, sp, #288",
        "bl {enter}",
        "cbz x0, 2f",
        "str x0, [sp, #8]",
        "2:",
        "ldp q6, q7, [sp, #256]",
        "ldp q4, q5, [sp, #224]",
        "ldp q2, q3, [sp, #192]",
        "ldp q0, q1, [sp, #160]",
        "ldp x16, x18, [sp, #144]",
        "ldp x14, x15, [sp, #128]",
        "ldp x12, x13, [sp, #112]",
        "ldp x10, x11, [sp, #96]",
        "ldp x8, x9, [sp, #80]",
        "ldp x6, x7, [sp, #64]",
        "ldp x4, x5, [sp, #48]",
        "ldp x2, x3, [sp, #32]",
        "ldp x0, x1, [sp, #16]",
        "ldp x29, x30, [sp], #288",
        "ldr x16, [x16, #16]",
        "br x16",
        enter = sym span_enter,
    )
}

/// Exit trampoline reached when a [`span_stub`]-hooked callee executes `ret`. Preserves every
/// AAPCS64 / Rust return register (`x0..x8`, `q0..q7`, `x18`, `x29`), pops the thread's
/// [`blaze_launch::telemetry::ShadowStack`] via [`span_leave`], emits `SpanExit`, and returns
/// to `real_lr`.
///
/// # Safety
///
/// Reached only via a callee returning after [`span_entry`] installed `x30 = span_exit`.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn span_exit() {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-240]!",
        "mov x29, sp",
        "stp x0, x1, [sp, #16]",
        "stp x2, x3, [sp, #32]",
        "stp x4, x5, [sp, #48]",
        "stp x6, x7, [sp, #64]",
        "stp x8, x18, [sp, #80]",
        "stp q0, q1, [sp, #96]",
        "stp q2, q3, [sp, #128]",
        "stp q4, q5, [sp, #160]",
        "stp q6, q7, [sp, #192]",
        "add x0, sp, #240",
        "bl {leave}",
        "str x0, [sp, #8]",
        "ldp q6, q7, [sp, #192]",
        "ldp q4, q5, [sp, #160]",
        "ldp q2, q3, [sp, #128]",
        "ldp q0, q1, [sp, #96]",
        "ldp x8, x18, [sp, #80]",
        "ldp x6, x7, [sp, #64]",
        "ldp x4, x5, [sp, #48]",
        "ldp x2, x3, [sp, #32]",
        "ldp x0, x1, [sp, #16]",
        "ldp x29, x30, [sp], #240",
        "ret",
        leave = sym span_leave,
    )
}

#[cfg(target_arch = "aarch64")]
std::thread_local! {
    static SHADOW: core::cell::RefCell<blaze_launch::telemetry::ShadowStack> =
        const { core::cell::RefCell::new(blaze_launch::telemetry::ShadowStack::new()) };
}

#[cfg(target_arch = "aarch64")]
extern "C" fn span_enter(stub: usize, real_lr: usize, caller_sp: usize) -> usize {
    // SAFETY: `stub` is a live 24-byte `span_stub` whose bytes `8..16` hold `u64::from(slot)`.
    let slot = blaze_launch::Slot(unsafe { ((stub + 8) as *const u64).read() } as u32);
    let tid = crate::telemetry::current_thread_id();
    let ts = crate::telemetry::now_ns();
    let outcome = SHADOW.try_with(|cell| {
        let mut stack = cell.borrow_mut();
        stack.enter(slot, real_lr as u64, caller_sp as u64, ts, |unwound| {
            crate::telemetry::record_unwound_span();
            let _ = crate::telemetry::emit(crate::telemetry::Event::span_unwound(
                tid,
                unwound.slot,
                ts,
                unwound.duration_ns,
                unwound.span,
                unwound.parent,
            ));
        })
    });
    match outcome {
        Ok(blaze_launch::telemetry::EnterOutcome::Hooked { span, parent }) => {
            let _ = crate::telemetry::emit(crate::telemetry::Event::span_enter(
                tid,
                slot,
                ts,
                real_lr as u64,
                span,
                parent,
            ));
            span_exit as *const () as usize
        }
        Ok(blaze_launch::telemetry::EnterOutcome::Overflow) => {
            crate::telemetry::record_shadow_overflow();
            0
        }
        Err(_) => 0,
    }
}

#[cfg(target_arch = "aarch64")]
extern "C" fn span_leave(current_sp: usize) -> usize {
    let tid = crate::telemetry::current_thread_id();
    let ts = crate::telemetry::now_ns();
    let left = SHADOW.try_with(|cell| {
        let mut stack = cell.borrow_mut();
        stack.leave(current_sp as u64, ts, |unwound| {
            crate::telemetry::record_unwound_span();
            let _ = crate::telemetry::emit(crate::telemetry::Event::span_unwound(
                tid,
                unwound.slot,
                ts,
                unwound.duration_ns,
                unwound.span,
                unwound.parent,
            ));
        })
    });
    match left {
        Ok(Ok(span)) => {
            let _ = crate::telemetry::emit(crate::telemetry::Event::span_exit(
                tid,
                span.slot,
                ts,
                span.duration_ns,
                span.span,
                span.parent,
            ));
            span.real_lr as usize
        }
        // Should never happen: `span_exit` is only installed when `span_enter` pushed.
        _ => std::process::abort(),
    }
}

/// Restores every `[fp + 8] == span_exit` saved return address on the current thread's AArch64
/// frame-pointer chain back to its authentic `real_lr` from the thread's shadow stack and
/// emits [`crate::telemetry::EventKind::SpanUnwound`] events. Called right before
/// `_Unwind_RaiseException` so libunwind walks authentic return addresses with zero custom
/// unwinder hacks.
#[cfg(target_arch = "aarch64")]
pub fn restore_unwind_frames() -> usize {
    const MAX_FRAMES: usize = 512;
    let exit_addr = span_exit as *const () as usize as u64;
    let tid = crate::telemetry::current_thread_id();
    let ts = crate::telemetry::now_ns();
    // Drain active shadow entries into a stack buffer (zero heap allocation, bounded to
    // SHADOW_DEPTH).
    let mut drained = [0u64; blaze_launch::telemetry::SHADOW_DEPTH];
    let mut drained_len = 0usize;
    let _ = SHADOW.try_with(|cell| {
        let mut stack = cell.borrow_mut();
        if stack.depth() == 0 {
            return;
        }
        stack.unwind_all(ts, |u| {
            if drained_len < drained.len() {
                drained[drained_len] = u.real_lr;
                drained_len += 1;
            }
            crate::telemetry::record_unwound_span();
            let _ = crate::telemetry::emit(crate::telemetry::Event::span_unwound(
                tid,
                u.slot,
                ts,
                u.duration_ns,
                u.span,
                u.parent,
            ));
        });
    });
    if drained_len == 0 {
        return 0;
    }
    let Ok((low, high)) = crate::sys::stack() else {
        return 0;
    };
    let mut fp: usize;
    // SAFETY: reads current thread's AArch64 frame pointer `x29`.
    unsafe { core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
    let mut next_drained = 0usize;
    let mut restored = 0usize;
    for _ in 0..MAX_FRAMES {
        if fp == 0 || fp % 8 != 0 || fp < low || fp.saturating_add(16) > high {
            break;
        }
        // SAFETY: `fp..fp + 16` is 8-aligned inside this thread's live stack.
        let prev_fp = unsafe { (fp as *const usize).read() };
        let saved_lr_ptr = (fp + 8) as *mut u64;
        let saved_lr = unsafe { saved_lr_ptr.read() };
        if saved_lr == exit_addr {
            // Skip tail-call siblings whose saved `real_lr` was itself `span_exit`.
            while next_drained < drained_len && drained[next_drained] == exit_addr {
                next_drained += 1;
            }
            if next_drained < drained_len {
                // SAFETY: restoring the authentic caller return address in this thread's own
                // stack frame record before libunwind walks it.
                unsafe { saved_lr_ptr.write(drained[next_drained]) };
                next_drained += 1;
                restored += 1;
            }
        }
        if prev_fp <= fp {
            break;
        }
        fp = prev_fp;
    }
    restored
}

/// Launcher-owned wrapper for `_Unwind_RaiseException` bound by `run_mode/bind.rs`
/// (`LauncherOwn`). Restores any active `span_exit` frames on this thread before delegating to
/// the real system unwinder, so panics and `catch_unwind` work seamlessly through `span_stub`.
///
/// # Safety
///
/// Called with the standard Itanium ABI `_Unwind_Exception` pointer.
#[cfg(target_arch = "aarch64")]
pub unsafe extern "C" fn unwind_raise_exception(exception: *mut core::ffi::c_void) -> core::ffi::c_int {
    let _ = restore_unwind_frames();
    #[cfg(target_os = "macos")]
    let raw = unsafe {
        blaze_sys::posix::thread::dlsym(
            blaze_sys::posix::thread::RTLD_DEFAULT,
            c"_Unwind_RaiseException".as_ptr(),
        )
    };
    #[cfg(not(target_os = "macos"))]
    let raw = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_Unwind_RaiseException".as_ptr()) };
    if raw.is_null() {
        std::process::abort();
    }
    // SAFETY: `raw` is the system's `_Unwind_RaiseException`.
    let real: unsafe extern "C" fn(*mut core::ffi::c_void) -> core::ffi::c_int =
        unsafe { core::mem::transmute(raw) };
    unsafe { real(exception) }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    #[cfg(target_os = "macos")]
    use blaze_sys::posix::thread;

    use super::*;
    use crate::sys::Code;

    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn quad(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// The count stub's instructions and its body `.quad`, against known encodings.
    #[test]
    fn a_count_stub_encodes_to_known_bytes() {
        let bytes = count(0x1000, 0x2000, 0xdead_beef_0000_0008).expect("in reach");
        let expect: [(usize, u32, &str); 11] = [
            (0, 0xd284_0010, "movz x16, #0x2000"),
            (16, 0xb940_0211, "ldr w17, [x16]"),
            (20, 0x3400_00b1, "cbz w17, .+20"),
            (24, 0x5100_0631, "sub w17, w17, #1"),
            (28, 0xb900_0211, "str w17, [x16]"),
            (32, 0x5800_00d0, "ldr x16, .+24"),
            (36, 0xd61f_0200, "br x16"),
            (40, 0x5280_0031, "movz w17, #1"),
            (44, 0xb900_0a11, "str w17, [x16, #8]"),
            (48, 0x5800_0050, "ldr x16, .+8"),
            (52, 0xd61f_0200, "br x16"),
        ];
        for (at, encoding, what) in expect {
            assert_eq!(word(&bytes, at), encoding, "{what} at {at}");
        }
        assert_eq!(body_offset(Kind::Count), 56);
        assert_eq!(quad(&bytes, 56), 0xdead_beef_0000_0008);
    }

    /// The mark stub's instructions and its body `.quad`.
    #[test]
    fn a_mark_stub_encodes_to_known_bytes() {
        let bytes = mark(0x1000, 8, 0x12_3456, 0x1234_5678_0000_0010).expect("in reach");
        #[cfg(target_os = "macos")]
        assert_eq!(word(&bytes, 0), 0xd53b_d070, "mrs x16, TPIDRRO_EL0");
        #[cfg(target_os = "linux")]
        assert_eq!(word(&bytes, 0), 0xd53b_d050, "mrs x16, TPIDR_EL0");
        let expect: [(usize, u32, &str); 9] = [
            (4, 0x927d_f210, "and x16, x16, #~7"),
            (8, 0xf940_0610, "ldr x16, [x16, #8]"),
            (12, 0x9111_5a10, "add x16, x16, #0x456"),
            (16, 0x9144_8e10, "add x16, x16, #0x123, lsl 12"),
            (20, 0x5280_0031, "movz w17, #1"),
            (24, 0x3900_0211, "strb w17, [x16]"),
            (28, 0x5800_0070, "ldr x16, .+12"),
            (32, 0xd61f_0200, "br x16"),
            (36, 0, "udf #0"),
        ];
        for (at, encoding, what) in expect {
            assert_eq!(word(&bytes, at), encoding, "{what} at {at}");
        }
        assert_eq!(body_offset(Kind::Mark), 40);
        assert_eq!(quad(&bytes, 40), 0x1234_5678_0000_0010);
    }

    /// A function id past two 12-bit immediates is refused, not truncated.
    #[test]
    fn a_mark_stub_refuses_an_id_past_its_reach() {
        assert!(mark(0x1000, 8, MARK_SLOTS - 1, 0).is_ok());
        assert!(mark(0x1000, 8, MARK_SLOTS, 0).is_err());
    }

    /// The hook stub: `adr x16, .; b hook_entry; .quad body`; and the veneer.
    #[test]
    fn a_hook_stub_and_a_veneer_encode_to_known_bytes() {
        let bytes = hook_stub(0x1000, 0x2000, 0xbeef).expect("in reach");
        assert_eq!(word(&bytes, 0), 0x1000_0010, "adr x16, .");
        // (0x2000 - 0x1004) / 4 = 0x3ff.
        assert_eq!(word(&bytes, 4), 0x1400_03ff, "b .+0xffc");
        assert_eq!(quad(&bytes, body_offset(Kind::Hook)), 0xbeef);
        assert!(hook_stub(0x1000, 0x1004 + (1 << 27), 0).is_err(), "128 MiB forward is out of reach");
        assert!(hook_stub(0x1000, 0x1004 + (1 << 27) - 4, 0).is_ok());

        let bytes = veneer(0x1122_3344_5566_7788);
        assert_eq!(word(&bytes, 0), 0x5800_0051, "ldr x17, .+8");
        assert_eq!(word(&bytes, 4), 0xd61f_0220, "br x17");
        assert_eq!(quad(&bytes, 8), 0x1122_3344_5566_7788);
    }

    /// Eight integer and eight floating-point arguments, folded so that any one changed or
    /// two swapped changes the sum.
    #[expect(clippy::too_many_arguments, reason = "every argument register is the point")]
    extern "C" fn sixteen(
        a: u64,
        b: u64,
        c: u64,
        d: u64,
        e: u64,
        f: u64,
        g: u64,
        h: u64,
        p: f64,
        q: f64,
        r: f64,
        s: f64,
        t: f64,
        u: f64,
        v: f64,
        w: f64,
    ) -> u64 {
        let mut sum = 0u64;
        for x in [a, b, c, d, e, f, g, h] {
            sum = sum.wrapping_mul(31).wrapping_add(x);
        }
        for x in [p, q, r, s, t, u, v, w] {
            sum = sum.wrapping_mul(31).wrapping_add(x.to_bits());
        }
        sum
    }

    type Sixteen = extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, f64, f64, f64, f64, f64, f64, f64, f64) -> u64;

    /// Returned through x8: larger than 16 bytes, so AAPCS64 passes its address in x8.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[repr(C)]
    struct Wide {
        words: [u64; 6],
    }

    #[expect(clippy::too_many_arguments, reason = "every argument register beside x8 is the point")]
    extern "C" fn wide(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, p: f64) -> Wide {
        Wide { words: [a ^ h, b ^ g, c ^ f, d ^ e, p.to_bits(), a.wrapping_add(h).wrapping_mul(3)] }
    }

    type WideFn = extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, f64) -> Wide;

    /// Returns x8 in x0: what a body sees in the indirect-result register.
    #[unsafe(naked)]
    extern "C" fn echo_x8() {
        core::arch::naked_asm!("mov x0, x8", "ret")
    }

    /// The stubs of each kind in one mapping in front of `body`, and what they write.
    struct Rig {
        code: Code,
        count: u64,
        #[cfg(target_os = "macos")]
        mark: u64,
        hook: u64,
        span: u64,
        /// The pthread key whose value is the bitmap base (macOS).
        #[cfg(target_os = "macos")]
        key: thread::pthread_key_t,
    }

    const MARKED: u32 = 5;
    const BITMAP: usize = 64;

    impl Rig {
        fn new(body: u64) -> Rig {
            let page = crate::sys::page().expect("a page");
            let mut code = Code::map_with_data(page, page).expect("a mapping");
            let base = code.base() as u64;
            let data = code.data_base();
            // SAFETY: `data` is the data region's writable base (`Code::data_base`), a whole
            // page, zero until written; the counter is its first 16 bytes and nothing else
            // in this test writes them.
            unsafe { (data as *mut Counter).write(Counter::new(3)) };

            let count_at = 0u64;
            let stub = count(base + count_at, data as u64, body).expect("in reach");
            code.write(count_at as usize, &stub).expect("count stub written");

            #[cfg(target_os = "macos")]
            let (mark_at, key) = {
                let key = thread::key_create(None).expect("a pthread key");
                // SAFETY: key is live with no destructor. The bitmap stays live with the rig.
                assert_eq!(unsafe { thread::set_specific(key, (data + BITMAP) as *const _) }, Ok(()));
                let at = 64u64;
                let tp_off = u32::try_from(key * 8).expect("a TSD offset");
                let stub = mark(base + at, tp_off, MARKED, body).expect("in reach");
                code.write(at as usize, &stub).expect("mark stub written");
                (at, key)
            };

            // hook_entry is in the test binary, maybe beyond ±128 MiB: through a veneer.
            let veneer_at = 128u64;
            code.write(veneer_at as usize, &veneer(hook_entry as *const () as usize as u64)).expect("veneer written");
            let hook_at = 192u64;
            let stub = hook_stub_slot(base + hook_at, base + veneer_at, 42, body).expect("in reach");
            code.write(hook_at as usize, &stub).expect("hook stub written");

            // span_entry through its own veneer.
            let span_veneer_at = 256u64;
            code.write(span_veneer_at as usize, &veneer(span_entry as *const () as usize as u64)).expect("span veneer written");
            let span_at = 320u64;
            let sstub = span_stub(base + span_at, base + span_veneer_at, 77, body).expect("in reach");
            code.write(span_at as usize, &sstub).expect("span stub written");

            Rig {
                code,
                count: base + count_at,
                #[cfg(target_os = "macos")]
                mark: base + mark_at,
                hook: base + hook_at,
                span: base + span_at,
                #[cfg(target_os = "macos")]
                key,
            }
        }

        /// The address of the stub of `kind`, after checking the instructions it runs as
        /// written in the run view against words hand-assembled (clang's assembler,
        /// 2026-11-15), so a wrong encoding fails here instead of jumping wild.
        fn stub(&self, kind: Kind) -> u64 {
            let read = |at: u64, n: usize| -> [u32; 10] {
                let mut words = [0u32; 10];
                for (i, word) in words.iter_mut().take(n).enumerate() {
                    // SAFETY: inside the run view, live with `code`, written by `Rig::new`.
                    *word = unsafe { ((at as usize + 4 * i) as *const u32).read() };
                }
                words
            };
            match kind {
                Kind::Count => {
                    let words = read(self.count + 16, 10);
                    let hand = [
                        0xb940_0211, 0x3400_00b1, 0x5100_0631, 0xb900_0211, 0x5800_00d0, 0xd61f_0200, 0x5280_0031,
                        0xb900_0a11, 0x5800_0050, 0xd61f_0200,
                    ];
                    assert_eq!(words, hand, "the count stub as it will run");
                    self.count
                }
                #[cfg(target_os = "macos")]
                Kind::Mark => {
                    let words = read(self.mark, 10);
                    // Word 2 is `ldr x16, [x16, #8 * key]`; MARKED is 5, so word 4 adds 0 << 12.
                    let ldr = 0xf940_0210 | (u32::try_from(self.key).expect("a key") << 10);
                    let hand = [
                        0xd53b_d070, 0x927d_f210, ldr, 0x9100_1610, 0x9140_0210, 0x5280_0031, 0x3900_0211,
                        0x5800_0070, 0xd61f_0200, 0,
                    ];
                    assert_eq!(words, hand, "the mark stub as it will run");
                    self.mark
                }
                #[cfg(not(target_os = "macos"))]
                Kind::Mark => panic!("the mark stub runs only on macOS here"),
                Kind::Hook => {
                    let words = read(self.hook, 1);
                    assert_eq!(words[0], 0x1000_0010, "adr x16, .");
                    let veneer = read(self.hook - 64, 2);
                    assert_eq!(&veneer[..2], &[0x5800_0051, 0xd61f_0220], "the veneer as it will run");
                    self.hook
                }
                Kind::Span => {
                    let words = read(self.span, 1);
                    assert_eq!(words[0], 0x1000_0010, "adr x16, .");
                    let veneer = read(self.span - 64, 2);
                    assert_eq!(&veneer[..2], &[0x5800_0051, 0xd61f_0220], "the span veneer as it will run");
                    self.span
                }
                Kind::Alloc => panic!("alloc_stub is tested directly in the_alloc_stub_tracks_custom_global_allocator_and_dedups_system_malloc"),
            }
        }

        fn counter(&self) -> Counter {
            // SAFETY: the counter is the data region's first 16 bytes, live with `code`.
            unsafe { (self.code.data_base() as *const Counter).read() }
        }

        #[cfg(target_os = "macos")]
        fn marked(&self) -> u8 {
            // SAFETY: the bitmap is in the data region, live with `code`; byte MARKED of it.
            unsafe { ((self.code.data_base() + BITMAP + MARKED as usize) as *const u8).read() }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = thread::key_delete(self.key);
        }
    }

    /// The count stub counts down from its threshold, queues once it finds zero, and every
    /// call still reaches the body.
    #[test]
    fn a_count_stub_counts_down_and_queues_and_reaches_the_body() {
        let rig = Rig::new(sixteen as *const () as usize as u64);
        let at = rig.stub(Kind::Count);
        // SAFETY: the count stub, checked by `Rig::stub`, forwards every argument register to
        // `sixteen` unchanged.
        let call: Sixteen = unsafe { core::mem::transmute::<usize, Sixteen>(at as usize) };
        for n in 1..=5u32 {
            assert_eq!(call(1, 2, 3, 4, 5, 6, 7, 8, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0), sixteen(1, 2, 3, 4, 5, 6, 7, 8, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0));
            let seen = rig.counter();
            assert_eq!(seen.left, 3u32.saturating_sub(n), "counted down after call {n}");
            assert_eq!(seen.queued, u32::from(n > 3), "queued only once it found zero (call {n})");
        }
    }

    /// Through the stub of `kind`, sixteen argument registers reach the body as a direct call
    /// passes them; returns the rig to check what the stub wrote.
    fn sixteen_through(kind: Kind) -> Rig {
        let rig = Rig::new(sixteen as *const () as usize as u64);
        let at = rig.stub(kind);
        let args = (
            0x0101_0101_0101_0101u64,
            0x0202,
            u64::MAX,
            0x8000_0000_0000_0000,
            5,
            6,
            7,
            0xfeed_face,
            -1.5f64,
            2.25,
            f64::MIN_POSITIVE,
            1e300,
            -0.0,
            3.5,
            7.125,
            f64::MAX,
        );
        let direct = sixteen(args.0, args.1, args.2, args.3, args.4, args.5, args.6, args.7, args.8, args.9, args.10, args.11, args.12, args.13, args.14, args.15);
        // SAFETY: the stub was checked against its hand-assembled words by `Rig::stub`; it
        // forwards every argument register to `sixteen` unchanged; the mapping is live with
        // `rig`.
        let call: Sixteen = unsafe { core::mem::transmute::<usize, Sixteen>(at as usize) };
        let through = call(args.0, args.1, args.2, args.3, args.4, args.5, args.6, args.7, args.8, args.9, args.10, args.11, args.12, args.13, args.14, args.15);
        assert_eq!(through, direct, "through the {kind:?} stub");
        rig
    }

    /// Through the stub of `kind`, x8 reaches the body unchanged, and a struct returned
    /// through the x8 address arrives whole.
    fn x8_through(kind: Kind) {
        let echo = Rig::new(echo_x8 as *const () as usize as u64);
        let at = echo.stub(kind);
        let sent = 0x5a5a_0000_1234_5678u64 ^ at;
        let got: u64;
        // SAFETY: the stub was checked by `Rig::stub` and forwards to `echo_x8`, which returns
        // x8 in x0 and touches nothing else; the C convention's clobbers (x30 among them)
        // cover what the stub, the veneer and the hook trampoline write.
        unsafe {
            core::arch::asm!(
                "blr {f}",
                f = in(reg) at,
                inout("x8") sent => _,
                lateout("x0") got,
                clobber_abi("C"),
            );
        }
        assert_eq!(got, sent, "x8 through the {kind:?} stub");

        let rig = Rig::new(wide as *const () as usize as u64);
        let at = rig.stub(kind);
        let direct = wide(1, 2, 3, 4, 5, 6, 7, 8, 9.5);
        // SAFETY: checked by `Rig::stub`; forwards every argument register and x8 to `wide`.
        let call: WideFn = unsafe { core::mem::transmute::<usize, WideFn>(at as usize) };
        assert_eq!(call(1, 2, 3, 4, 5, 6, 7, 8, 9.5), direct, "a struct by pointer through the {kind:?} stub");
    }

    #[test]
    fn the_count_stub_passes_eight_integer_and_eight_float_arguments() {
        let rig = sixteen_through(Kind::Count);
        assert_eq!(rig.counter().left, 2, "the count stub counted");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mark_stub_passes_eight_integer_and_eight_float_arguments() {
        let rig = sixteen_through(Kind::Mark);
        assert_eq!(rig.marked(), 1, "the mark stub set its byte");
    }

    #[test]
    fn the_hook_stub_passes_eight_integer_and_eight_float_arguments() {
        let hooked = HOOKED.load(core::sync::atomic::Ordering::Relaxed);
        sixteen_through(Kind::Hook);
        assert!(HOOKED.load(core::sync::atomic::Ordering::Relaxed) > hooked, "the hook ran");
    }

    #[test]
    fn the_count_stub_keeps_x8_and_a_struct_returned_by_pointer() {
        x8_through(Kind::Count);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mark_stub_keeps_x8_and_a_struct_returned_by_pointer() {
        x8_through(Kind::Mark);
    }

    #[test]
    fn the_hook_stub_keeps_x8_and_a_struct_returned_by_pointer() {
        x8_through(Kind::Hook);
    }

    #[test]
    fn the_span_stub_passes_eight_integer_and_eight_float_arguments() {
        sixteen_through(Kind::Span);
    }

    #[test]
    fn the_span_stub_keeps_x8_and_a_struct_returned_by_pointer() {
        x8_through(Kind::Span);
    }

    /// Twelve integer arguments: 8 in x0..x7 and 4 on the caller's stack `[sp + 0..32]`.
    /// Proves `span_entry` leaves `sp` completely untouched for stack-passed arguments!
    #[expect(clippy::too_many_arguments, reason = "proves stack-passed arguments past x7 survive span_entry")]
    extern "C" fn twelve_ints(
        a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64,
        s0: u64, s1: u64, s2: u64, s3: u64,
    ) -> u64 {
        a.wrapping_mul(1)
            .wrapping_add(b.wrapping_mul(2))
            .wrapping_add(c.wrapping_mul(3))
            .wrapping_add(d.wrapping_mul(5))
            .wrapping_add(e.wrapping_mul(7))
            .wrapping_add(f.wrapping_mul(11))
            .wrapping_add(g.wrapping_mul(13))
            .wrapping_add(h.wrapping_mul(17))
            .wrapping_add(s0.wrapping_mul(19))
            .wrapping_add(s1.wrapping_mul(23))
            .wrapping_add(s2.wrapping_mul(29))
            .wrapping_add(s3.wrapping_mul(31))
    }

    #[test]
    fn the_span_stub_preserves_stack_arguments_and_records_nested_spans() {
        let _guard = crate::telemetry::TEST_GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Allocate an aligned in-memory telemetry ring and install it globally for this test.
        let cap = 256u32;
        let bytes = crate::telemetry::Telemetry::bytes_for(cap).expect("bytes");
        let layout = core::alloc::Layout::from_size_align(bytes, 128).expect("layout");
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let base = core::ptr::NonNull::new(raw).expect("nonnull");
        let ring = unsafe { crate::telemetry::Telemetry::init(base, bytes, cap) }.expect("init");
        crate::telemetry::install_global(base, bytes).expect("install");

        // 1. 12 integer args (4 on stack!) through Kind::Span.
        let rig = Rig::new(twelve_ints as *const () as usize as u64);
        let at = rig.stub(Kind::Span);
        type Twelve = extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
        let direct = twelve_ints(1, 2, 3, 4, 5, 6, 7, 8, 100, 200, 300, 400);
        let call: Twelve = unsafe { core::mem::transmute::<usize, Twelve>(at as usize) };
        let got = call(1, 2, 3, 4, 5, 6, 7, 8, 100, 200, 300, 400);
        assert_eq!(got, direct, "stack arguments must survive span_entry/span_exit unchanged");

        // 2. Nested call: outer (slot 10) calls inner (slot 20) through two span stubs.
        extern "C" fn inner_fn(x: u64) -> u64 {
            x.wrapping_mul(3).wrapping_add(1)
        }
        static INNER_STUB_ADDR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
        extern "C" fn outer_fn(x: u64) -> u64 {
            let f: extern "C" fn(u64) -> u64 = unsafe {
                core::mem::transmute(INNER_STUB_ADDR.load(core::sync::atomic::Ordering::Acquire))
            };
            f(x) + 10
        }

        let page = crate::sys::page().expect("page");
        let mut code = Code::map(page).expect("code");
        let cbase = code.base() as u64;
        code.write(0, &veneer(span_entry as *const () as usize as u64)).expect("veneer");
        let outer_stub = span_stub(cbase + 64, cbase, 10, outer_fn as *const () as usize as u64).expect("outer");
        let inner_stub = span_stub(cbase + 128, cbase, 20, inner_fn as *const () as usize as u64).expect("inner");
        code.write(64, &outer_stub).expect("write outer");
        code.write(128, &inner_stub).expect("write inner");
        INNER_STUB_ADDR.store((cbase + 128) as usize, core::sync::atomic::Ordering::Release);

        // Drain the twelve_ints events first (filtered to this thread so concurrent thread tests don't interfere).
        let my_tid = crate::telemetry::current_thread_id();
        let mut events = Vec::new();
        ring.drain(64, |e| {
            if e.thread == my_tid {
                events.push(e);
            }
        });
        assert_eq!(events.len(), 2, "twelve_ints emits SpanEnter + SpanExit");
        assert_eq!(events[0].event_kind(), Some(crate::telemetry::EventKind::SpanEnter));
        assert_eq!(events[1].event_kind(), Some(crate::telemetry::EventKind::SpanExit));
        assert_eq!(events[0].slot_or_op, 77);
        assert_eq!(events[0].b, events[1].b, "matching SpanId on enter and exit");

        events.clear();
        let outer_call: extern "C" fn(u64) -> u64 = unsafe { core::mem::transmute((cbase + 64) as usize) };
        assert_eq!(outer_call(5), 26);

        ring.drain(64, |e| {
            if e.thread == my_tid {
                events.push(e);
            }
        });
        assert_eq!(events.len(), 4, "outer+inner emits 4 span events: {events:?}");
        assert_eq!(events[0].event_kind(), Some(crate::telemetry::EventKind::SpanEnter));
        assert_eq!(events[0].slot_or_op, 10);
        assert_eq!(events[1].event_kind(), Some(crate::telemetry::EventKind::SpanEnter));
        assert_eq!(events[1].slot_or_op, 20);
        assert_eq!(events[1].c, events[0].b, "inner's parent SpanId must equal outer's SpanId");
        assert_eq!(events[2].event_kind(), Some(crate::telemetry::EventKind::SpanExit));
        assert_eq!(events[2].slot_or_op, 20);
        assert_eq!(events[3].event_kind(), Some(crate::telemetry::EventKind::SpanExit));
        assert_eq!(events[3].slot_or_op, 10);

        // 3. Deep recursion past SHADOW_DEPTH (256) returns exact value and records overflows.
        static REC_STUB_ADDR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
        extern "C" fn recurse(n: u64) -> u64 {
            if n == 0 {
                return 0;
            }
            let f: extern "C" fn(u64) -> u64 = unsafe {
                core::mem::transmute(REC_STUB_ADDR.load(core::sync::atomic::Ordering::Acquire))
            };
            1 + f(n - 1)
        }
        let rec_stub = span_stub(cbase + 192, cbase, 30, recurse as *const () as usize as u64).expect("rec");
        code.write(192, &rec_stub).expect("write rec");
        REC_STUB_ADDR.store((cbase + 192) as usize, core::sync::atomic::Ordering::Release);
        let rec_call: extern "C" fn(u64) -> u64 = unsafe { core::mem::transmute((cbase + 192) as usize) };
        let depth = (blaze_launch::telemetry::SHADOW_DEPTH as u64) + 20;
        assert_eq!(rec_call(depth), depth, "deep recursion past SHADOW_DEPTH must compute exact result");
        assert!(ring.stats().shadow_overflows >= 20, "overflows must be counted, not silent");

        // 4. Panic + catch_unwind through span_stub after restore_unwind_frames restores LR.
        extern "C-unwind" fn panicking_callee(trigger: u64) -> u64 {
            if trigger != 0 {
                let restored = restore_unwind_frames();
                assert!(restored >= 1, "must restore at least one span_exit frame on stack");
                panic!("intentional panic through span_stub");
            }
            99
        }
        let panic_stub = span_stub(cbase + 256, cbase, 40, panicking_callee as *const () as usize as u64).expect("panic stub");
        code.write(256, &panic_stub).expect("write panic stub");
        let panic_call: extern "C-unwind" fn(u64) -> u64 = unsafe { core::mem::transmute((cbase + 256) as usize) };
        let caught = std::panic::catch_unwind(|| panic_call(1));
        assert!(caught.is_err(), "panic through span_stub must be caught cleanly by catch_unwind");
        assert!(ring.stats().unwound_spans >= 1, "unwound span must be counted");
        // Subsequent normal call after caught panic still works cleanly!
        assert_eq!(panic_call(0), 99);

        crate::telemetry::clear_global();
        unsafe { std::alloc::dealloc(raw, layout) };
    }

    /// Proves on real AArch64 hardware that [`alloc_stub`] + [`alloc_entry`]:
    /// 1. Tracks a completely custom bump/arena `#[global_allocator]` that NEVER touches libc
    ///    `malloc` across all 4 Rust allocator entry points (`__rust_alloc`, `__rust_alloc_zeroed`,
    ///    `__rust_realloc`, `__rust_dealloc`), capturing exact `(ptr, size, caller_frames)`.
    /// 2. Deduplicates `System`-style allocators calling `ObservedBind::malloc` underneath via
    ///    the shared `try_enter_alloc_hook` guard (emits 1 `RustAlloc` event, NOT 2).
    #[test]
    fn the_alloc_stub_tracks_custom_global_allocator_and_dedups_system_malloc() {
        use blaze_launch::telemetry::{AllocHookKind, AllocOp, EventKind};
        use core::sync::atomic::{AtomicUsize, Ordering};

        let _guard = crate::telemetry::TEST_GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_stride = crate::telemetry::alloc_sample_bytes();
        crate::telemetry::set_alloc_sample_bytes(0); // exact mode for assertion

        let ring_bytes = crate::telemetry::Telemetry::bytes_for(64).expect("bytes");
        let layout = std::alloc::Layout::from_size_align(ring_bytes, 128).expect("layout");
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let base = core::ptr::NonNull::new(raw).expect("non-null");
        let ring = unsafe { crate::telemetry::Telemetry::init(base, ring_bytes, 64) }.expect("init");
        crate::telemetry::install_global(base, ring_bytes).expect("install");

        let mut code = crate::sys::Code::map(1 << 16).expect("code mapping");
        let cbase = code.base() as u64;
        let v = veneer(alloc_entry as *const () as usize as u64);
        code.write(0, &v).expect("write alloc veneer");

        // 1. Custom bump allocator (never touches libc malloc!).
        static BUMP_BUF: [u8; 4096] = [0; 4096];
        static BUMP_OFF: AtomicUsize = AtomicUsize::new(0);
        static FREED_SUM: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn custom_alloc(size: usize, align: usize) -> *mut u8 {
            let cur = BUMP_OFF.load(Ordering::Relaxed);
            let aligned = (cur + align.max(1) - 1) & !(align.max(1) - 1);
            BUMP_OFF.store(aligned + size, Ordering::Relaxed);
            unsafe { BUMP_BUF.as_ptr().add(aligned) as *mut u8 }
        }
        extern "C" fn custom_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
            custom_alloc(size, align)
        }
        extern "C" fn custom_realloc(_ptr: *mut u8, _old: usize, align: usize, new_size: usize) -> *mut u8 {
            custom_alloc(new_size, align)
        }
        extern "C" fn custom_dealloc(_ptr: *mut u8, size: usize, _align: usize) {
            FREED_SUM.fetch_add(size, Ordering::Relaxed);
        }

        code.write(32, &alloc_stub(cbase + 32, cbase, AllocHookKind::Alloc, custom_alloc as *const () as usize as u64).expect("s1")).expect("w1");
        code.write(64, &alloc_stub(cbase + 64, cbase, AllocHookKind::AllocZeroed, custom_alloc_zeroed as *const () as usize as u64).expect("s2")).expect("w2");
        code.write(96, &alloc_stub(cbase + 96, cbase, AllocHookKind::Realloc, custom_realloc as *const () as usize as u64).expect("s3")).expect("w3");
        code.write(128, &alloc_stub(cbase + 128, cbase, AllocHookKind::Dealloc, custom_dealloc as *const () as usize as u64).expect("s4")).expect("w4");

        let f_alloc: extern "C" fn(usize, usize) -> *mut u8 = unsafe { core::mem::transmute((cbase + 32) as usize) };
        let f_zeroed: extern "C" fn(usize, usize) -> *mut u8 = unsafe { core::mem::transmute((cbase + 64) as usize) };
        let f_realloc: extern "C" fn(*mut u8, usize, usize, usize) -> *mut u8 = unsafe { core::mem::transmute((cbase + 96) as usize) };
        let f_dealloc: extern "C" fn(*mut u8, usize, usize) = unsafe { core::mem::transmute((cbase + 128) as usize) };

        let p1 = f_alloc(128, 16);
        let p2 = f_zeroed(256, 32);
        let p3 = f_realloc(p1, 128, 16, 512);
        f_dealloc(p3, 512, 16);
        assert_eq!(FREED_SUM.load(Ordering::Relaxed), 512);

        let my_tid = crate::telemetry::current_thread_id();
        let mut events = Vec::new();
        ring.drain(64, |e| {
            if e.thread == my_tid {
                events.push(e);
            }
        });
        assert_eq!(events.len(), 4, "expected 4 custom allocator events: {events:?}");
        assert_eq!(events[0].event_kind(), Some(EventKind::Alloc));
        assert_eq!(events[0].slot_or_op, AllocOp::RustAlloc as u32);
        assert_eq!(events[0].a, p1 as usize as u64);
        assert_eq!(events[0].b, 128);
        assert_ne!(events[0].c, 0, "must capture caller return address");

        assert_eq!(events[1].slot_or_op, AllocOp::RustAllocZeroed as u32);
        assert_eq!(events[1].a, p2 as usize as u64);
        assert_eq!(events[1].b, 256);

        assert_eq!(events[2].slot_or_op, AllocOp::RustRealloc as u32);
        assert_eq!(events[2].a, p3 as usize as u64);
        assert_eq!(events[2].b, 512);

        assert_eq!(events[3].event_kind(), Some(EventKind::Free));
        assert_eq!(events[3].slot_or_op, AllocOp::RustDealloc as u32);
        assert_eq!(events[3].a, p3 as usize as u64);
        assert_eq!(events[3].b, 512);

        // 2. System-style allocator calling ObservedBind's `malloc` underneath: MUST NOT double-count!
        use crate::run_mode::{Bind, Dlsym, ObservedBind};
        let mut obs = ObservedBind::new(Dlsym);
        let traced_malloc_addr = obs.intercept("malloc").expect("ObservedBind intercepts malloc");
        static TRACED_MALLOC_PTR: AtomicUsize = AtomicUsize::new(0);
        TRACED_MALLOC_PTR.store(traced_malloc_addr as usize, Ordering::Release);

        extern "C" fn system_like_alloc(size: usize, _align: usize) -> *mut u8 {
            let m: extern "C" fn(usize) -> *mut core::ffi::c_void =
                unsafe { core::mem::transmute(TRACED_MALLOC_PTR.load(Ordering::Acquire)) };
            m(size) as *mut u8
        }
        code.write(160, &alloc_stub(cbase + 160, cbase, AllocHookKind::Alloc, system_like_alloc as *const () as usize as u64).expect("s5")).expect("w5");
        let f_sys: extern "C" fn(usize, usize) -> *mut u8 = unsafe { core::mem::transmute((cbase + 160) as usize) };

        events.clear();
        let sys_ptr = f_sys(384, 8);
        assert!(!sys_ptr.is_null());
        unsafe { libc::free(sys_ptr.cast()) };

        ring.drain(64, |e| {
            if e.thread == my_tid {
                events.push(e);
            }
        });
        assert_eq!(
            events.len(),
            1,
            "Rust __rust_alloc -> ObservedBind::malloc must emit EXACTLY 1 event (no double-counting!): {events:?}"
        );
        assert_eq!(events[0].slot_or_op, AllocOp::RustAlloc as u32);
        assert_eq!(events[0].b, 384);

        crate::telemetry::set_alloc_sample_bytes(prev_stride);
        crate::telemetry::clear_global();
        unsafe { std::alloc::dealloc(raw, layout) };
    }
}
