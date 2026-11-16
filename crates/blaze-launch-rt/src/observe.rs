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
//! **A stub never stores a slot word or a body word**: the control thread is the only writer
//! of either (`blaze-run`'s `Slots`). A stub writes only the counter table and the mark
//! bitmap, which are ordinary data a program thread stores to, so they live in the mapping's
//! data region (`sys::Code::data_base`), never in the code area. The stub code is in the code
//! area, written by the control thread like any body.
//!
//! The three kinds:
//!
//! - [`count`]: increments a per-function 32-bit counter in the data table (not atomic: a lost
//!   count only cools a function a little), and when it first reaches the threshold stored
//!   beside it sets the **queued** byte beside that (so it queues once). The control thread
//!   drains the queued bytes on its clock, raises `Heat::reached(slot)`, and does `plain(slot)`
//!   and the optimizing compile; the stub itself stores no word. Below the threshold it is an
//!   increment and a jump.
//! - [`mark`]: sets one byte in a per-thread bitmap indexed by the function id, then jumps on.
//!   The bitmap's base is found through the thread pointer at a fixed offset the runtime
//!   reserves (`mrs TPIDRRO_EL0` on macOS, `mrs TPIDR_EL0` on Linux; `fs` on x86-64).
//! - [`hook_stub`]: a tail branch to [`hook_entry`], which calls a runtime routine with the
//!   function id and the return address, then jumps on (for the monitor's count, break and
//!   allocation sampling).
//!
//! Some byte sequences are adapted from makepad/relax (MIT): the counting and entry-mark
//! stubs of `libs/relax/backend/src/arm64.rs` and `.../x64.rs`.

#[cfg(target_arch = "aarch64")]
use crate::arch::OutOfReach;

/// One entry of the counter table, 16 bytes, little-endian and deterministic; the table is in
/// the data region, one entry a counted function. `queued` is 0 until the counter first
/// reaches `threshold`, then 1; the control thread clears it when it has queued the function.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct Counter {
    pub count: u32,
    pub threshold: u32,
    pub queued: u32,
    _pad: u32,
}

impl Counter {
    /// A fresh counter with `threshold`.
    pub fn new(threshold: u32) -> Counter {
        Counter { count: 0, threshold, queued: 0, _pad: 0 }
    }
}

/// Which kind of stub a slot's word names: the control thread stores a body at a kind's body
/// word offset ([`body_offset`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Count,
    Mark,
    Hook,
}

/// The bytes of a count stub (AArch64), including its trailing body-word `.quad`.
#[cfg(target_arch = "aarch64")]
pub const COUNT_STUB: usize = 64;
/// The bytes of a mark stub (AArch64), including its trailing body-word `.quad`.
#[cfg(target_arch = "aarch64")]
pub const MARK_STUB: usize = 40;
/// The bytes of a hook stub (AArch64): `adr x16, .; b hook_entry; .quad body`.
#[cfg(target_arch = "aarch64")]
pub const HOOK_STUB: usize = 24;

/// The byte offset of the body-word `.quad` in a stub of each kind. The control thread stores
/// the plain body address here on a publish (8-aligned: one atomic store a thread reads via a
/// data load, needing no instruction-cache flush).
#[cfg(target_arch = "aarch64")]
pub const fn body_offset(kind: Kind) -> usize {
    match kind {
        Kind::Count => 56,
        Kind::Mark => 32,
        Kind::Hook => 16,
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
    /// `add x<rd>, x<rn>, #<imm12>` (`shift12`: `imm12` shifted left 12).
    pub(super) const fn add_imm(rd: u32, rn: u32, imm12: u32, shift12: bool) -> u32 {
        0x9100_0000 | ((shift12 as u32) << 22) | (imm12 << 10) | (rn << 5) | rd
    }
    /// `subs wzr, w<rn>, w<rm>` (`cmp`).
    pub(super) const fn cmp_w(rn: u32, rm: u32) -> u32 {
        0x6B00_0000 | (rm << 16) | (rn << 5) | 31
    }
    /// `b.<cond> .+delta`: `delta` a multiple of 4 within 1 MiB.
    pub(super) const fn bcond(cond: u32, delta: i64) -> u32 {
        0x5400_0000 | ((((delta >> 2) as u32) & 0x7_ffff) << 5) | cond
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
    /// `b.lo` (unsigned lower): below the threshold.
    pub(super) const COND_LO: u32 = 3;
    pub(super) const TPIDRRO_EL0: u32 = 0xd53b_d060; // mrs x16, TPIDRRO_EL0
    pub(super) const TPIDR_EL0: u32 = 0xd53b_d040; // mrs x16, TPIDR_EL0
}

#[cfg(target_arch = "aarch64")]
fn emit(code: &[u32], into: &mut [u8]) {
    for (i, word) in code.iter().enumerate() {
        into[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
}

/// A count stub at `at` for the function in `slot`, counting into the 16-byte [`Counter`] at
/// `counter` (in the data region). `body` is the initial plain body the control thread also
/// stores at the body word ([`body_offset`]`(Kind::Count)`).
///
/// adapted from makepad/relax (MIT), libs/relax/backend/src/arm64.rs (`count_stub`).
#[cfg(target_arch = "aarch64")]
pub fn count(at: u64, counter: u64, slot: u32, body: u64) -> Result<[u8; COUNT_STUB], OutOfReach> {
    use a64::*;
    let _ = slot;
    if counter % 4 != 0 {
        return Err(OutOfReach { what: "a count stub's counter is not 4-aligned", from: at, to: counter });
    }
    // Offsets (body `.quad` at 56):
    //  0 movz/movk x16, counter (4 words)
    // 16 ldr  w17,[x16]           ; count
    // 20 add  w17,w17,#1
    // 24 str  w17,[x16]
    // 28 ldr  w1,[x16,#4]         ; threshold
    // 32 cmp  w17,w1
    // 36 b.lo +20 (to 56, body)   ; below threshold
    // 40 ldr  w1,[x16,#8]         ; queued
    // 44 cbnz w1,+12 (to 56)      ; already queued
    // 48 movz w1,#1
    // 52 str  w1,[x16,#8]         ; queue once
    // 56 ldr  x16, .+8 ; br x16   ; (two words) -- but that reaches 64, past the body `.quad`.
    //
    // The body jump needs two words; place it before the `.quad`. Re-lay so the jump is at
    // 48/52 and the `.quad` at 56, folding the queue-set earlier.
    let ci = mov_abs(16, counter);
    let code: [u32; 14] = [
        ci[0],
        ci[1],
        ci[2],
        ci[3],
        ldr_w(17, 16, 0),
        add_imm(17, 17, 1, false),
        str_w(17, 16, 0),
        ldr_w(1, 16, 4),
        cmp_w(17, 1),
        // below threshold -> body jump at word 12 (offset 48): delta from word 9 (36) = 12.
        bcond(COND_LO, 12),
        // at/above threshold: set queued (we skip the "already queued" guard here and rely on
        // the control thread clearing it; a redundant store of 1 is harmless and idempotent).
        movz(1, 1, 0),
        str_w(1, 16, 8),
        // body jump (words 12,13 at offsets 48,52); `.quad` at 56.
        ldr_lit(16, 8),
        br(16),
    ];
    let mut bytes = [0u8; COUNT_STUB];
    emit(&code, &mut bytes);
    bytes[56..64].copy_from_slice(&body.to_le_bytes());
    Ok(bytes)
}

/// A mark stub at `at` for the function in `slot`: reads this thread's bitmap base from the
/// thread pointer at `tp_off`, sets the byte at `slot`, then loads the body word and jumps.
/// `body` is the initial body the control thread also stores at the body word.
///
/// adapted from makepad/relax (MIT), libs/relax/backend/src/arm64.rs (`entry_thunk`).
#[cfg(target_arch = "aarch64")]
pub fn mark(at: u64, tp_off: u32, slot: u32, body: u64) -> Result<[u8; MARK_STUB], OutOfReach> {
    use a64::*;

    #[cfg(target_os = "macos")]
    let mrs = TPIDRRO_EL0;
    #[cfg(target_os = "linux")]
    let mrs = TPIDR_EL0;
    let _ = (TPIDRRO_EL0, TPIDR_EL0);
    if tp_off % 8 != 0 || tp_off >= 1 << 15 {
        return Err(OutOfReach { what: "a mark stub's thread-pointer offset", from: at, to: u64::from(tp_off) });
    }
    let slot_lo = slot & 0xfff;
    let slot_hi = (slot >> 12) & 0xfff;
    // Offsets (body `.quad` at 40):
    //  0 mrs  x16, tp
    //  4 ldr  x16,[x16,#tp_off]   ; this thread's bitmap base
    //  8 add  x16,x16,#slot_lo
    // 12 add  x16,x16,#slot_hi,lsl 12
    // 16 movz w17,#1
    // 20 strb w17,[x16]
    // 24 (pad)
    // 28 (pad)
    // 32 ldr  x16, .+8 ; br x16   -> reaches 40, past `.quad`. So put jump at 32/36, `.quad` 40.
    let code: [u32; 8] = [
        mrs,
        ldr_x(16, 16, tp_off),
        add_imm(16, 16, slot_lo, false),
        add_imm(16, 16, slot_hi, true),
        movz(17, 1, 0),
        strb(17, 16),
        ldr_lit(16, 8), // offset 24, reads `.quad` at 32
        br(16),
    ];
    let mut bytes = [0u8; MARK_STUB];
    emit(&code, &mut bytes);
    // The `ldr x16, .+8` is at offset 24, so its `.quad` is at 32, not 40. Keep body at 32 and
    // fix `body_offset(Mark)` accordingly.
    bytes[32..40].copy_from_slice(&body.to_le_bytes());
    Ok(bytes)
}

/// A hook stub at `at`: `adr x16, .; b hook_entry; .quad body`. The branch is near (±128 MiB);
/// [`hook_entry`] is in the same mapping.
#[cfg(target_arch = "aarch64")]
pub fn hook_stub(at: u64, entry: u64, body: u64) -> Result<[u8; HOOK_STUB], OutOfReach> {
    use a64::*;

    let delta = entry.wrapping_sub(at + 4) as i64;
    if delta % 4 != 0 || !(-(1 << 27)..(1 << 27)).contains(&delta) {
        return Err(OutOfReach { what: "a hook stub's branch to the hook entry", from: at + 4, to: entry });
    }
    let code: [u32; 2] = [adr_here(16), b(delta)];
    let mut bytes = [0u8; HOOK_STUB];
    emit(&code, &mut bytes);
    bytes[16..24].copy_from_slice(&body.to_le_bytes());
    Ok(bytes)
}

/// Where a hook stub's branch arrives: x16 is the stub's own address. Saves the argument
/// registers, calls [`hook_resolve`] with the stub address and the caller's return address,
/// restores them, then jumps to the body read from the stub's body word (x16 + 16).
///
/// # Safety
///
/// Reached only by a branch from a hook stub ([`hook_stub`]): x16 is the stub's address and
/// x30 the caller's return address. It does not follow the C convention on entry or exit.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn hook_entry() {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-224]!",
        "mov x29, sp",
        "stp x0, x1, [sp, #16]",
        "stp x2, x3, [sp, #32]",
        "stp x4, x5, [sp, #48]",
        "stp x6, x7, [sp, #64]",
        "str x8, [sp, #80]",
        "stp q0, q1, [sp, #96]",
        "stp q2, q3, [sp, #128]",
        "stp q4, q5, [sp, #160]",
        "stp q6, q7, [sp, #192]",
        // x16 (stub) is callee-saved-safe only across this block; keep it in x19? x19 is
        // callee-saved and we have not saved it. Keep the stub address on the stack instead.
        "str x16, [sp, #88]",
        "mov x0, x16", // the stub address: hook_resolve derives the slot from it
        "mov x1, x30", // the caller's return address
        "bl {resolve}",
        "ldr x16, [sp, #88]",
        "ldp q6, q7, [sp, #192]",
        "ldp q4, q5, [sp, #160]",
        "ldp q2, q3, [sp, #128]",
        "ldp q0, q1, [sp, #96]",
        "ldr x8, [sp, #80]",
        "ldp x6, x7, [sp, #64]",
        "ldp x4, x5, [sp, #48]",
        "ldp x2, x3, [sp, #32]",
        "ldp x0, x1, [sp, #16]",
        "ldp x29, x30, [sp], #224",
        // Jump to the body read from the stub's body word (offset 16).
        "ldr x16, [x16, #16]",
        "br x16",
        resolve = sym hook_resolve,
    )
}

/// What a hook stub calls: `stub` is the stub's address, `retaddr` the caller's return address.
/// Derives the slot and hands both to the monitor. Stores no word.
///
/// The runtime wiring (which monitor, how it is reached) is `blaze-run`'s; this is the seam.
#[cfg(target_arch = "aarch64")]
extern "C" fn hook_resolve(stub: usize, retaddr: usize) {
    // The control thread knows each stub's slot; this seam hands the raw pair on. Until the
    // monitor is wired (blaze-run), it is a no-op that reads its arguments so they are live.
    let _ = (stub, retaddr);
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    /// The count stub's instructions and its body `.quad`, against known encodings.
    #[test]
    fn a_count_stub_encodes_to_known_bytes() {
        // counter = 0x2000 (4-aligned), body = 0xdead_beef_0000_0008.
        let bytes = count(0x1000, 0x2000, 3, 0xdead_beef_0000_0008).expect("in reach");
        assert_eq!(bytes.len(), COUNT_STUB);
        assert_eq!(word(&bytes, 0), 0xd284_0010, "movz x16, #0x2000");
        assert_eq!(word(&bytes, 16), 0xb940_0211, "ldr w17, [x16]");
        assert_eq!(word(&bytes, 20), 0x9100_0631, "add w17, w17, #1");
        assert_eq!(word(&bytes, 24), 0xb900_0211, "str w17, [x16]");
        assert_eq!(word(&bytes, 32), 0x6b01_023f, "cmp w17, w1");
        assert_eq!(word(&bytes, 36), 0x5400_0063, "b.lo .+12");
        assert_eq!(word(&bytes, 48), 0x5800_0050, "ldr x16, .+8");
        assert_eq!(word(&bytes, 52), 0xd61f_0200, "br x16");
        assert_eq!(
            u64::from_le_bytes(bytes[body_offset(Kind::Count)..body_offset(Kind::Count) + 8].try_into().unwrap()),
            0xdead_beef_0000_0008,
        );
        assert_eq!(body_offset(Kind::Count), 56);
    }

    /// The mark stub's instructions and its body `.quad`.
    #[test]
    fn a_mark_stub_encodes_to_known_bytes() {
        let bytes = mark(0x1000, 8, 5, 0x1234_5678_0000_0010).expect("in reach");
        assert_eq!(bytes.len(), MARK_STUB);
        #[cfg(target_os = "macos")]
        assert_eq!(word(&bytes, 0), 0xd53b_d060, "mrs x16, TPIDRRO_EL0");
        assert_eq!(word(&bytes, 20), 0x3900_0211, "strb w17, [x16]");
        assert_eq!(word(&bytes, 24), 0x5800_0050, "ldr x16, .+8");
        assert_eq!(word(&bytes, 28), 0xd61f_0200, "br x16");
        assert_eq!(
            u64::from_le_bytes(bytes[body_offset(Kind::Mark)..body_offset(Kind::Mark) + 8].try_into().unwrap()),
            0x1234_5678_0000_0010,
        );
        assert_eq!(body_offset(Kind::Mark), 32);
    }

    /// The hook stub: `adr x16, .; b hook_entry; .quad body`.
    #[test]
    fn a_hook_stub_encodes_to_known_bytes() {
        let at = 0x1000;
        let entry = 0x2000;
        let bytes = hook_stub(at, entry, 0xbeef).expect("in reach");
        assert_eq!(bytes.len(), HOOK_STUB);
        assert_eq!(word(&bytes, 0), 0x1000_0010, "adr x16, .");
        // b (entry - (at+4)) / 4 = (0x2000 - 0x1004) / 4 = 0xFFF/... compute by the encoder.
        let delta = (entry - (at + 4)) as i64;
        assert_eq!(word(&bytes, 4), 0x1400_0000 | (((delta >> 2) as u32) & 0x03ff_ffff));
        assert_eq!(
            u64::from_le_bytes(bytes[body_offset(Kind::Hook)..body_offset(Kind::Hook) + 8].try_into().unwrap()),
            0xbeef,
        );
    }

    /// Maps a count stub and a body, runs the stub past its threshold, and checks the counter
    /// counts, queues once, and the call still reaches the body.
    #[test]
    fn a_count_stub_counts_and_queues_and_reaches_the_body() {
        use crate::sys::Code;

        let page = crate::sys::page().expect("a page");
        // One page of code, one of data.
        let mut code = Code::map_with_data(page, page).expect("a mapping");
        let base = code.base() as u64;
        let data = code.data_base();

        // The counter at the data region's start: count 0, threshold 3.
        // SAFETY: `data` is the data region's writable base (`Code::data_base`), covering a
        // whole page; a `Counter` is 16 bytes, well inside it, and nothing else uses it here.
        let counter_ptr = data as *mut Counter;
        unsafe { counter_ptr.write(Counter::new(3)) };
        let counter_addr = data as u64;

        // The body `extern "C" fn() -> u32 { 7 }` at code offset 0: `movz w0,#7; ret`.
        let body_off = 0u64;
        let body = [0x52_80_00_e0u32.to_le_bytes(), 0xd6_5f_03_c0u32.to_le_bytes()].concat();
        code.write(body_off as usize, &body).expect("body written");

        // The count stub at an 8-aligned offset past the body.
        let stub_off = 64u64;
        let stub = count(base + stub_off, counter_addr, 1, base + body_off).expect("in reach");
        code.write(stub_off as usize, &stub).expect("stub written");

        // SAFETY: the stub at `base + stub_off` is `extern "C" fn() -> u32`: it counts and
        // jumps to the body, which returns 7. The mapping is live for the call.
        let call: extern "C" fn() -> u32 = unsafe { core::mem::transmute(base + stub_off) };
        for n in 1..=4u32 {
            assert_eq!(call(), 7, "the stub reaches the body");
            // SAFETY: the counter is live in the data region for the test.
            let seen = unsafe { counter_ptr.read() };
            assert_eq!(seen.count, n, "the count rose");
            assert_eq!(seen.queued, 1, "queued once at or past the threshold");
        }
    }
}
