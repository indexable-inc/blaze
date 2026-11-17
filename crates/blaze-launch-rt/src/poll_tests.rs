//! The poll's fault path ([`crate::poll`]) in simulation: a sealed fast record with one loop,
//! its loop map of every place kind and 0 to 12 rows (so both register and stack words), a
//! thread stopped at the poll's load, and a model stack. Checked: each word lands where the
//! convention says (loop_entry.rs "The convention"), the caller's callee-saved registers come
//! back from the save area, the thread resumes at the variant with `x30` the exit stub; then
//! the exit stub's three instructions, modelled, leave `sp`, `x29`, `x30` as the fast
//! function's caller had them. A fault anywhere but a poll's load is refused by name.

use blaze_code::unwind::Frame;
use blaze_own::Vec;
use blaze_tier::record::{encode, read, seal};
use blaze_tier::{Emitted, Live, LoopHeader, Place, Poll, Reg};

use crate::poll::{
    ARG_REGS, PlacedRecords, PollError, StackMemory, ThreadRegs, apply, on_poll_fault,
};

const BASE: u64 = 0x10_0000;
const VARIANT: u64 = 0x20_0000;
const STUB: u64 = 0x30_0000;
/// The model stack: `LOW..LOW + 8 * WORDS`.
const LOW: u64 = 0x7000_0000;
const WORDS: usize = 512;

struct Stack {
    words: [u64; WORDS],
}

impl Stack {
    fn index(at: u64) -> Option<usize> {
        let off = at.checked_sub(LOW)?;
        (off % 8 == 0 && (off / 8) < WORDS as u64).then_some((off / 8) as usize)
    }
}

impl StackMemory for Stack {
    fn read(&self, at: u64) -> Option<u64> {
        Stack::index(at).map(|i| self.words[i])
    }
    fn write(&mut self, at: u64, word: u64) -> Option<()> {
        let i = Stack::index(at)?;
        self.words[i] = word;
        Some(())
    }
}

struct One<'b> {
    bytes: &'b [u8],
}

impl PlacedRecords for One<'_> {
    fn holding(&self, pc: u64) -> Option<(u64, blaze_tier::record::RecordView<'_>)> {
        let view = read(self.bytes).expect("the sealed record reads");
        (pc >= BASE && pc < BASE + view.code().len() as u64).then_some((BASE, view))
    }
    fn variant(&self, base: u64, header: u32) -> Option<u64> {
        (base == BASE && header == 0).then_some(VARIANT)
    }
    fn exit_stub(&self) -> u64 {
        STUB
    }
}

/// Row `i`'s place: a register (x19 + i for the first, then caller-saved x9.., then a `d`
/// register), a cell or a slot, by `i % 4`.
fn place_of(i: u32) -> Place {
    match i % 4 {
        0 if i == 0 => Place::Reg(Reg(19)),
        0 => Place::Reg(Reg(9 + (i / 4) as u8)),
        1 => Place::Cell { sp_offset: 16 * i },
        2 => Place::Slot { sp_offset: 16 * i },
        _ => Place::Reg(Reg(32 + 16 + (i / 4) as u8)),
    }
}

/// A sealed record: six words of code, the poll at 8 (its load at 12), one loop at 0 with
/// `rows` live values keyed `3 i`, and the pair x19:x20 saved.
fn record(rows: u32) -> std::vec::Vec<u8> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut e = Emitted::new_in(alloc);
    e.code.extend_from_slice(&[0u8; 24]);
    e.frame = 16;
    e.unwind_frame = Some(Frame { x_pairs: 1, d_pairs: 0 });
    for i in 0..rows {
        e.live.push(Live { value: 3 * i, place: place_of(i) });
    }
    e.loops.push(LoopHeader { at: 0, first: 0, count: rows });
    e.polls.push(Poll { at: 8, header: 0 });
    let sealed = seal(e).expect("the record seals");
    let mut out = Vec::new_in(alloc);
    encode(&sealed, &mut out);
    out.to_vec()
}

/// For 0 to 12 rows: every word where the convention puts it, the saved pair given back, the
/// thread at the variant; then the exit stub's model returns to the caller. Expected: passes.
#[test]
fn a_poll_fault_resumes_in_the_variant_with_the_loop_map() {
    for rows in 0..=12u32 {
        let bytes = record(rows);
        let records = One { bytes: &bytes };
        let mut stack = Stack { words: [0; WORDS] };
        // The fast frame: the caller's sp is LOW + 2048; its frame record at fp = sp - 16;
        // the pair x19:x20 below it in the one frame form (`stp x20, x19`: x20 at fp - 16, x19
        // at fp - 8); the fast sp 256 bytes lower.
        let caller_sp = LOW + 2048;
        let fp = caller_sp - 16;
        let sp = fp - 256;
        stack.write(fp, 0xCA11_E2F9).expect("in the stack");
        stack.write(fp + 8, 0xCA11_E2A0).expect("in the stack");
        stack.write(fp - 16, 0x2020).expect("in the stack");
        stack.write(fp - 8, 0x1919).expect("in the stack");
        let mut regs = ThreadRegs { sp, pc: BASE + 12, ..ThreadRegs::default() };
        regs.x[29] = fp;
        regs.x[30] = 0xDEAD;
        let mut expected = std::vec::Vec::new();
        for i in 0..rows {
            let word = 0x1000 + u64::from(i);
            match place_of(i) {
                Place::Reg(r) if r.0 < 32 => regs.x[r.hw() as usize] = word,
                Place::Reg(r) => regs.d[r.hw() as usize] = word,
                Place::Cell { sp_offset } => stack.write(sp + u64::from(sp_offset), word).expect("in the stack"),
                Place::Slot { .. } => {}
            }
            expected.push(match place_of(i) {
                Place::Slot { sp_offset } => sp + u64::from(sp_offset),
                _ => word,
            });
        }
        let resume = on_poll_fault(regs.pc, &mut regs, &records).expect("the poll's fault resumes");
        apply(&resume, &mut regs, &mut stack).expect("the moves apply");
        assert_eq!((regs.pc, regs.x[30], regs.x[29]), (VARIANT, STUB, fp), "rows {rows}");
        assert_eq!(regs.sp % 16, 0, "rows {rows}");
        assert!(regs.sp <= sp, "the variant's frame is above the fast frame");
        for (i, &word) in expected.iter().enumerate() {
            let got = match i < ARG_REGS {
                true => regs.x[i],
                false => stack.read(regs.sp + 8 * (i - ARG_REGS) as u64).expect("in the stack"),
            };
            assert_eq!(got, word, "rows {rows}: word {i}");
        }
        assert_eq!((regs.x[19], regs.x[20]), (0x1919, 0x2020), "rows {rows}: the caller's x19:x20");
        // The exit stub, modelled: `mov sp, x29; ldp x29, x30, [sp], #16; ret`.
        let mut sp_after = regs.x[29];
        let (x29, x30) = (stack.read(sp_after).expect("in"), stack.read(sp_after + 8).expect("in"));
        sp_after += 16;
        assert_eq!((sp_after, x29, x30), (caller_sp, 0xCA11_E2F9, 0xCA11_E2A0), "rows {rows}");
    }
    println!("poll: 0 to 12 rows resume in the variant and return to the caller");
}

/// A fault at the poll's first instruction, or past the code, is no poll: refused by name and
/// `regs` untouched. Expected: `NotAPoll { offset: 8 }`, then `NotPlaced`.
#[test]
fn a_fault_off_a_polls_load_is_refused() {
    let bytes = record(2);
    let records = One { bytes: &bytes };
    let mut regs = ThreadRegs { sp: LOW + 1024, pc: BASE + 8, ..ThreadRegs::default() };
    let before = regs;
    assert_eq!(on_poll_fault(BASE + 8, &mut regs, &records), Err(PollError::NotAPoll { offset: 8 }));
    assert_eq!(on_poll_fault(BASE + 64, &mut regs, &records), Err(PollError::NotPlaced { pc: BASE + 64 }));
    assert_eq!(regs, before);
    println!("poll: a fault off a poll's load refused");
}
