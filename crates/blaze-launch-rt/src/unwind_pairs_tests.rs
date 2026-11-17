//! The ONE frame form against the REAL unwinder (unwind-P8a-4; main's ruling: the platform's
//! order, blaze-code unwind.rs "The frame word"): a panic raised below two placed functions
//! whose frames save x19..x22 is unwound through them by the system's unwinder, reading each
//! frame from the entry blaze-code eh.rs wrote for its record, and the registers the unwinder
//! restores are the ones the frame saved.
//!
//! The placed code (one page, two records):
//! - `outer(boom, inner)` at 0: frame record, saves x19..x22 as the fast tier's composer does
//!   (`stp x20, x19, [sp, #-16]!` then `stp x22, x21, [sp, #-16]!`: blaze-emit compose/emit.rs
//!   SAVE_PAIR, register at the higher address first), puts KNOWN values in x19..x22, calls
//!   `inner(boom)` with a cleanup row (so the test's personality is asked in the cleanup phase),
//!   then restores and returns.
//! - `inner(boom)` at 64: the same frame, CLOBBERS x19..x22, calls `boom`, which panics.
//!
//! In the cleanup phase the unwinder has stepped out of `inner` by `inner`'s entry: the values
//! it gives for x19..x22 in `outer`'s frame (`_Unwind_GetGR`) are read from `inner`'s save area
//! where the entry says they are. They equal `outer`'s known values only if the code and the
//! entry agree on where each register of a pair is. The panic then goes on to `catch_unwind`
//! above `outer`.
//!
//! The control is planted in the same test: `inner` placed with the OLD order (`stp x19, x20`,
//! lower register at the lower address) under the same entry gives the pairs back swapped
//! (x19 reads x20's value), which the first assertion would refuse.

#![cfg(target_arch = "aarch64")]

use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_code::eh::Placed;
use blaze_code::unwind::{Action, Frame, Row};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::Emitted;

use crate::unwind::{deregister, register};
use crate::unwind_reg::eh_bytes;

unsafe extern "C" {
    fn _Unwind_GetGR(context: *mut c_void, reg: c_int) -> usize;
}

const SEARCH_PHASE: c_int = 1;
const CONTINUE_UNWIND: c_int = 8;

/// `outer`'s known values for x19, x20, x21, x22.
const KNOWN: [u64; 4] = [0xA19, 0xA20, 0xA21, 0xA22];
/// What the unwinder gave for x19..x22 in `outer`'s frame, in the cleanup phase.
static SEEN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The test's root allocator capability (D102).
fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `outer`'s personality: no handler in the search phase; in the cleanup phase, x19..x22 as
/// the unwinder restored them for this frame are kept in [`SEEN`], and unwinding goes on.
unsafe extern "C" fn personality(_version: c_int, actions: c_int, _class: u64, _exception: *mut c_void, context: *mut c_void) -> c_int {
    if actions & SEARCH_PHASE != 0 {
        return CONTINUE_UNWIND;
    }
    for (k, seen) in SEEN.iter().enumerate() {
        // SAFETY: `context` is the unwinder's for this frame, valid during the call; 19..22
        // are the DWARF numbers of x19..x22.
        let value = unsafe { _Unwind_GetGR(context, 19 + k as c_int) };
        seen.store(value as u64, Ordering::SeqCst);
    }
    CONTINUE_UNWIND
}

extern "C-unwind" fn boom() {
    panic!("a panic below two placed frames that save x19..x22 (unwind_pairs_tests.rs)");
}

/// `movz x<rd>, #imm16`.
const fn movz(rd: u32, imm: u32) -> u32 {
    0xD280_0000 | (imm << 5) | rd
}

/// The two saves and the two restores: the platform's order (`stp x20, x19`, `stp x22, x21`;
/// `ldp x22, x21`, `ldp x20, x19`), or the old one for the control (`stp x19, x20`, ...).
fn saves(old: bool) -> ([u32; 2], [u32; 2]) {
    match old {
        false => ([0xA9BF_4FF4, 0xA9BF_57F6], [0xA8C1_57F6, 0xA8C1_4FF4]),
        true => ([0xA9BF_53F3, 0xA9BF_5BF5], [0xA8C1_5BF5, 0xA8C1_53F3]),
    }
}

/// One function: frame record, two saved pairs, x19..x22 set to `values`, `blr x<callee>`,
/// restores, `ret`, then `brk #0` at 52 (the cleanup row's landing, never installed).
fn function(values: [u32; 4], callee: u32, old: bool) -> [u8; 56] {
    let (save, restore) = saves(old);
    let words: [u32; 14] = [
        0xA9BF_7BFD, // 0: stp x29, x30, [sp, #-16]!
        0x9100_03FD, // 4: mov x29, sp
        save[0],     // 8: the pair x19:x20
        save[1],     // 12: the pair x21:x22
        movz(19, values[0]),
        movz(20, values[1]),
        movz(21, values[2]),
        movz(22, values[3]),
        0xD63F_0000 | (callee << 5), // 32: blr x<callee>
        restore[0],                  // 36
        restore[1],                  // 40
        0xA8C1_7BFD,                 // 44: ldp x29, x30, [sp], #16
        0xD65F_03C0,                 // 48: ret
        0xD420_0000,                 // 52: brk #0
    ];
    let mut out = [0u8; 56];
    for (i, word) in words.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

const INNER_AT: usize = 64;
const CALL: u32 = 32;
const LANDING: u32 = 52;

/// One run: `inner` placed with the old order when `old`, both entries in the one frame form.
/// Gives x19..x22 as the unwinder restored them in `outer`'s frame.
fn run(old: bool) -> [u64; 4] {
    let alloc = alloc();
    let page = crate::sys::page().expect("page size");
    let mut area = crate::sys::Code::map(page).expect("a code area");
    let base = area.base() as u64;
    let outer = function([0xA19, 0xA20, 0xA21, 0xA22], 1, false);
    let inner = function([0xB19, 0xB20, 0xB21, 0xB22], 0, old);
    // Both frames say the same: a frame record and the pairs x19:x20 and x21:x22.
    let frame = Frame { x_pairs: 0b11, d_pairs: 0 };

    let mut outer_rec = Emitted::new_in(alloc);
    outer_rec.code.extend_from_slice(&outer);
    outer_rec.unwind_frame = Some(frame);
    outer_rec.unwind.push(Row { start: CALL, len: 4, landing: LANDING, action: Action::Cleanup });
    let mut inner_rec = Emitted::new_in(alloc);
    inner_rec.code.extend_from_slice(&inner);
    inner_rec.unwind_frame = Some(frame);
    inner_rec.unwind.push(Row { start: CALL, len: 4, landing: 0, action: Action::Continue });

    // Fixed capacity: the entries' addresses are absolute, so their bytes never move.
    let mut outer_eh: AVec<'_, u8> = Vec::with_capacity_in(1024, alloc);
    let mut inner_eh: AVec<'_, u8> = Vec::with_capacity_in(1024, alloc);
    let (outer_eh_at, inner_eh_at) = (outer_eh.as_ptr() as u64, inner_eh.as_ptr() as u64);
    let at = Placed { entry_at: outer_eh_at, code_at: base, personality: Some(personality as usize as u64) };
    let outer_made = eh_bytes(&outer_rec, at, &mut outer_eh).expect("outer's entry");
    let at = Placed { entry_at: inner_eh_at, code_at: base + INNER_AT as u64, personality: None };
    let inner_made = eh_bytes(&inner_rec, at, &mut inner_eh).expect("inner's entry");
    assert_eq!((outer_eh.as_ptr() as u64, inner_eh.as_ptr() as u64), (outer_eh_at, inner_eh_at), "the entries stayed where they were made for");

    area.write(0, &outer).expect("outer written");
    area.write(INNER_AT, &inner).expect("inner written");
    // SAFETY: each entry's bytes are in its buffer and the code in `area`, all alive and
    // unchanged until `deregister` below.
    unsafe {
        register(outer_eh_at as usize, outer_made.entry.fde);
        register(inner_eh_at as usize, inner_made.entry.fde);
    }
    for seen in SEEN.iter() {
        seen.store(0, Ordering::SeqCst);
    }
    type Inner = extern "C-unwind" fn(extern "C-unwind" fn());
    // SAFETY: `base` holds `outer` as `function` wrote it: AAPCS64, two arguments (`boom` in
    // x0, `inner` in x1), a frame record, callee-saved registers kept; it unwinds only through
    // its registered entry, as `inner` does.
    let outer_fn: extern "C-unwind" fn(extern "C-unwind" fn(), Inner) =
        unsafe { core::mem::transmute::<usize, extern "C-unwind" fn(extern "C-unwind" fn(), Inner)>(base as usize) };
    // SAFETY: as above, for `inner` at `INNER_AT`: one argument, `boom`, in x0.
    let inner_fn: Inner = unsafe { core::mem::transmute::<usize, Inner>(base as usize + INNER_AT) };
    let caught = std::panic::catch_unwind(|| outer_fn(boom, inner_fn));
    assert!(caught.is_err(), "the panic reached catch_unwind above outer");
    // SAFETY: no thread is in the placed code (the call returned by unwinding).
    unsafe {
        deregister(inner_eh_at as usize, inner_made.entry.fde);
        deregister(outer_eh_at as usize, outer_made.entry.fde);
    }
    drop((outer_eh, inner_eh));
    [0, 1, 2, 3].map(|k| SEEN[k].load(Ordering::SeqCst))
}

/// Prints "unwind_pairs: the unwinder restored x19..x22 through a saved-pair frame; the old
/// order gives them back swapped". Fails if blaze-code's frame form and the composer's saves
/// disagree with each other or with the platform's unwinder (libunwind
/// `stepWithCompactEncodingFrame`'s order, which the DWARF entry must match).
#[test]
fn a_panic_through_saved_pairs_restores_them() {
    let restored = run(false);
    assert_eq!(restored, KNOWN, "x19..x22 in outer's frame, as the unwinder restored them through inner");
    let control = run(true);
    assert_eq!(control, [0xA20, 0xA19, 0xA22, 0xA21], "the old order (planted in inner) gives each pair back swapped");
    println!("unwind_pairs: the unwinder restored x19..x22 through a saved-pair frame; the old order gives them back swapped");
}
