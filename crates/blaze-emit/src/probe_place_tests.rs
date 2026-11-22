//! Where the stack probes live (pass/probe.rs, `place_frame`; THREADS.md D3, threads P5 redo).
//!
//! The rules. From the pinned library (std/src/sys/pal/unix/stack_overflow/handler_signal.rs,
//! 19-20 and 109): "We've got guard pages (unmapped pages) at the end of every thread's stack, so
//! if a thread ends up running into the guard page it'll trigger" ...; "Even for
//! panic=immediate-abort, installing the guard pages is important for soundness": so `sp` never
//! drops 4096 bytes (the smallest host page) past its last write without writing again. From the
//! code record's own invariant (record_check.rs, `no_address_word`): the record "checks the first
//! `insts` bytes of `view`'s code (the instructions)", and what follows them is data rows only
//! (record_tests.rs, `holds`): so the probe tail, being instructions, lies inside `insts`.
//!
//! Three tests: the tail inside the counted instructions, before the data
//! (`the_probe_tail_is_inside_the_record_instructions`); a slot's alignment counted in the drop
//! (`an_over_aligned_frame_counts_its_rounding`); the Fast tier's probe from its table
//! (`the_fast_tier_probes_a_frame_past_a_page`). pass/probe.rs's sequence itself is
//! probe_tests.rs's.

use std::alloc::Global;

use blaze_lir::{
    Builder, Flags, Full, Ty,
    builder::{MemAttrs, Variadic},
    func::{BlockKind, Conv, Func, SlotKind},
};
use blaze_own::Alloc;
use blaze_tier::{ARM64, Named, Names, Outside, Table, Tier};

use crate::{
    BULK_INLINE_MOST,
    compose::Fast,
    enc::{self, SP},
    enc_probe,
    pass::probe::PROBE_STEP,
    record::emit_record_up_to,
    table_gen::EncodedTable,
    tests::{ISA, emitted, one},
};

const STEP: u64 = PROBE_STEP as u64;

/// The prologue's frame words: `stp`, `add fp`, then the two at this offset (pass.rs, `run`).
const FRAME_AT: u64 = 8;

/// No reference has a name (none of these functions names one).
struct Nameless;

impl Names for Nameless {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// `f() = ret`, with one stack slot of `slot` bytes aligned to `1 << align` and nothing else:
/// its frame is one 16-byte cell (the `ret`), the slot at its alignment, rounded to 16, plus
/// `(1 << align) - 16` for the rounding of `sp` (pass.rs, `run`; pass/probe.rs, `frame_bytes`).
fn framed(slot: u32, align: u8) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let _slot = b.slot(slot, align, SlotKind::Temp);
    b.ret(&[]);
    b.finish_checked().expect("a hand-built function builds")
}

fn word(code: &[u8], at: u64) -> u32 {
    let at = at as usize;
    u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

/// What running the frame's words from `FRAME_AT` until the branch back to `FRAME_AT + 8` did.
struct Walk {
    /// Each write's address, in order: the `stp` at `entry - 16` first.
    touched: Vec<u64>,
    /// `sp` where the words branch back.
    sp: u64,
    /// The highest byte offset any walked word ended at.
    reached: u64,
}

/// Runs the words from `FRAME_AT` on `sp` and `x17` alone, reading only `code[..end]`; a word
/// outside `end` or one pass/probe.rs does not place is refused. Bounded: a frame under 16 MiB
/// takes at most 4096 turns of the loop.
fn walk(code: &[u8], end: u64, entry: u64) -> Walk {
    let mut touched = vec![entry - 16];
    let (mut sp, mut x17, mut hs, mut reached) = (entry - 16, 0u64, false, 0u64);
    let mut pc = FRAME_AT;
    for _ in 0..(8 * 4096 + 64) {
        assert!(pc + 4 <= end, "the frame's words run to {pc}, past the {end} bytes of instructions");
        reached = reached.max(pc + 4);
        let w = word(code, pc);
        let mut next = pc + 4;
        if w & 0xfc00_0000 == 0x1400_0000 {
            let to = pc.wrapping_add_signed(i64::from(((w << 6) as i32) >> 6) * 4);
            if to == FRAME_AT + 8 {
                return Walk { touched, sp, reached };
            }
            next = to;
        } else if w & 0xffe0_001f == 0xd280_0011 {
            x17 = u64::from((w >> 5) & 0xffff);
        } else if w & 0xffe0_001f == 0xf2a0_0011 {
            x17 = (x17 & !0xffff_0000) | (u64::from((w >> 5) & 0xffff) << 16);
        } else if w & 0xffc0_03ff == 0xd140_03ff {
            sp -= u64::from((w >> 10) & 0xfff) << 12;
        } else if w == 0xf900_03ff {
            touched.push(sp);
        } else if w & 0xffc0_03ff == 0xd140_0231 {
            x17 = x17.checked_sub(u64::from((w >> 10) & 0xfff) << 12).expect("the tail's count never goes below zero");
        } else if w & 0xffc0_03ff == 0xf140_023f {
            hs = x17 >= u64::from((w >> 10) & 0xfff) << 12;
        } else if w & 0xff00_001f == 0x5400_0002 {
            if hs {
                next = pc.wrapping_add_signed(i64::from(((w << 8) as i32) >> 13) * 4);
            }
        } else if w == 0xcb31_63ff {
            sp -= x17;
        } else {
            panic!("a word the probe tail does not hold at {pc}: {w:#010x}");
        }
        pc = next;
    }
    panic!("the probe tail ran past its bound without branching back");
}

/// Every stretch `sp` leaves untouched is under one step: between two writes, and under the
/// last one down to `lowest`, the least `sp` reaches.
fn under_a_step(what: &str, walk: &Walk, lowest: u64) {
    for pair in walk.touched.windows(2) {
        let (above, below) = (pair[0], pair[1]);
        assert!(below < above, "{what}: the writes go down");
        let gap = above - (below + 8);
        assert!(gap < STEP, "{what}: {gap} bytes untouched between writes at {above:#x} and {below:#x}");
    }
    let last = *walk.touched.last().expect("the `stp` wrote");
    let rest = last - lowest;
    assert!(rest < STEP, "{what}: {rest} bytes untouched under the last write");
}

/// The record's invariant: its first `insts` bytes are the instructions and only data rows
/// follow. A frame past a page, in a function that also places a data row: the record emits and
/// seals, the bytes after `insts` are no more than the row and its padding, and the words from
/// the prologue's `b tail` back to `frame_at + 8` all lie before `insts`. The first version
/// placed the tail after the data and fails here (and in record_tests.rs, `holds`: "36 bytes
/// follow the instructions, more than the data rows holding no address"). Prints "the probe
/// tail lies inside the record's instructions".
#[test]
fn the_probe_tail_is_inside_the_record_instructions() {
    let entry: u64 = 1 << 40;
    for slot in [4096u32, 8192, 1 << 20] {
        let func = one(&[], Ty::I8, |b, _| {
            let _slot = b.slot(slot, 4, SlotKind::Temp);
            let row = b.data(&[7, 8, 9], 0, &[]);
            let at = b.data_addr(row);
            b.load(Ty::I8, Flags::NONE, at, 1, MemAttrs::default())
        });
        let (record, insts, _) = emit_record_up_to(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &Nameless, &crate::RoutinePlaces::NONE, ISA, BULK_INLINE_MOST, Alloc::global())
            .unwrap_or_else(|why| panic!("a {slot}-byte slot's function emits as a record: {why}"));
        let code = &record.code[..];
        let insts = u64::from(insts);
        assert!(insts % 4 == 0 && insts as usize <= code.len(), "{insts} bytes of instructions in {}", code.len());
        let after = code.len() as u64 - insts;
        assert!(after <= 3 + 15, "a {slot}-byte slot: {after} bytes follow the instructions, more than the 3-byte row with padding");
        let first = word(code, FRAME_AT);
        assert_eq!(first & 0xfc00_0000, 0x1400_0000, "a {slot}-byte slot's prologue word is a `b`: {first:#010x}");
        assert_eq!(word(code, FRAME_AT + 4), enc_probe::NOP, "a {slot}-byte slot's second word is a `nop`");
        let walk = walk(code, insts, entry);
        assert!(walk.reached <= insts, "the tail ends inside the instructions");
        let frame = walk.touched[0] - walk.sp;
        assert!(frame > u64::from(slot), "a {slot}-byte slot's tail moves `sp` by its frame ({frame})");
        under_a_step(&format!("a {slot}-byte slot"), &walk, walk.sp);
        record.seal().unwrap_or_else(|why| panic!("a {slot}-byte slot's record does not seal: {why}"));
    }
    println!("the probe tail lies inside the record's instructions");
}

/// A slot aligned past 16 bytes: the prologue rounds `sp` down after the frame's `sub`s, which
/// drops it by up to `align - 16` more. A 2048-byte slot at 1 KiB: frame `3072 + 1008 = 4080`
/// (`16 + 4080` is at the bound, so a count of the frame alone pays nothing), drop up to
/// `4080 + 1008`: past a page, so it is probed and the tail touches the frame's bottom before the
/// rounding. A 16-byte slot at 1 KiB: frame `1040 + 1008 = 2048`, drop at most 3056: no probe,
/// the two `sub`s. The first version counted the frame alone and fails the first case. Prints
/// "an over-aligned frame counts its rounding".
#[test]
fn an_over_aligned_frame_counts_its_rounding() {
    let small = emitted(framed(16, 10).view(), &[]).expect("emits");
    assert_eq!(word(&small.bytes, FRAME_AT), enc::sub_imm(SP, SP, 0, true), "a 2048-byte frame keeps its first `sub`");
    assert_eq!(word(&small.bytes, FRAME_AT + 4), enc::sub_imm(SP, SP, 2048, false), "a 2048-byte frame keeps its second `sub`");

    let entry: u64 = 1 << 40;
    let code = emitted(framed(2048, 10).view(), &[]).expect("emits");
    let bytes = &code.bytes;
    let first = word(bytes, FRAME_AT);
    assert_eq!(first & 0xfc00_0000, 0x1400_0000, "a 4080-byte frame aligned to 1 KiB is probed: {first:#010x}");
    let walk = walk(bytes, bytes.len() as u64, entry);
    assert_eq!(walk.sp, entry - 16 - 4080, "the tail moves `sp` by the frame");
    // The rounding to 1 KiB drops at most 1008 bytes more, untouched.
    under_a_step("a 4080-byte frame aligned to 1 KiB", &walk, walk.sp - (1024 - 16));
    println!("an over-aligned frame counts its rounding");
}

/// The Fast tier probes a frame whose drop under the saved registers passes one page with the
/// table's `glue::PROBE` template (this file's sequence, its count in a lent scratch register),
/// and keeps the two `sub`s at the bound. A 4096-byte slot: frame 4096, `16 + 4096 > 4096`, so
/// the code holds the probe's `str xzr, [sp]` and `sub sp, sp, #1, lsl #12`; a 4064-byte slot:
/// frame 4064, neither. Running a probed Fast function is tier_tests/directed.rs's
/// (`a_far_frame_with_a_flag_live_across_stores_agrees`). Prints "the fast tier probes a frame
/// past a page". Fails with `TableError::Missing` for `stack_pointer` variant 8 until the table
/// is made again (`cargo run -p blaze-emit --example gen_table`).
#[test]
fn the_fast_tier_probes_a_frame_past_a_page() {
    let table = match EncodedTable::read(include_bytes!("../table/arm64.bztab"), Alloc::global()) {
        Ok(table) => table,
        Err(error) => panic!("the committed table does not read: {error}"),
    };
    let fast = Fast { table: &table, regs: &ARM64 };
    let machine = table.head().machine;
    let touch = enc::str(enc::Size::X, enc::ZR, SP, 0);
    let step = enc::sub_imm(SP, SP, 1, true);
    let words = |code: &[u8]| -> Vec<u32> { code.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect() };
    for slot in [4096u32, 40 * 1024, 1 << 20] {
        let func = framed(slot, 4);
        let large = fast.emit(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &Nameless, machine, Alloc::global());
        let e = match large {
            Ok(e) => e,
            Err(error) => panic!("the Fast tier refused a {slot}-byte slot's frame: {error}"),
        };
        let w = words(&e.code[..]);
        assert!(w.contains(&touch) && w.contains(&step), "a {slot}-byte slot's prologue probes: {w:x?}");
        assert!(e.frame >= slot, "a {slot}-byte slot's frame is {}", e.frame);
    }
    let bound_func = framed(4064, 4);
    match fast.emit(blaze_lir::verify(bound_func.view()).expect("the test's function is LIR"), &Nameless, machine, Alloc::global()) {
        Ok(e) => assert!(!words(&e.code[..]).contains(&touch), "a 4064-byte frame is within the bound: no probe"),
        Err(error) => panic!("the Fast tier refused a 4064-byte frame: {error}"),
    }
    println!("the fast tier probes a frame past a page");
}
