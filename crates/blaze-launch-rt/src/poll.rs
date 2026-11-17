//! The poll's fault handler (WAVE-BACKEND.md piece L; backend-next.md D98, D100 points 2 and 3):
//! a running loop tiers up at a back edge.
//!
//! Each back edge of a fast record runs the poll, `adrp x16, poll@PAGE; ldr xzr, [x16,
//! poll@PAGEOFF]` (blaze-tier record/mod.rs [`blaze_tier::Poll`]), a load from the runtime's poll
//! page. When the loop's instance has an optimized LOOP-ENTRY variant placed (blaze-lir
//! loop_entry.rs), the parent protects the page; the load faults, and the signal driver hands
//! the faulting thread's registers here. [`on_poll_fault`] finds the poll by the faulting
//! program counter (the poll's SECOND instruction, the load), the loop's header and its loop
//! map, and says how the thread resumes in the variant:
//!
//! - The variant takes one word per loop-map row, in the record's (key) order (loop_entry.rs
//!   "The convention"): a `Reg` row's word is the register (an `Fpr` home's low 64 bits), a
//!   `Cell` row's the first 8 bytes of its cell at `sp + sp_offset`, a `Slot` row's the
//!   slot's ADDRESS `sp + sp_offset` (the slot stays in the fast frame: D100 point 2). Word `i`
//!   goes to `x<i>` for `i < 8` (every word is an integer, so the handler needs no type), the
//!   rest to the stack at the new `sp` in 8-byte steps, as AAPCS64 and Apple's arm64 ABI both
//!   pass 8-byte integers.
//! - The variant runs in a NEW frame above the fast frame (D100 point 2): `sp` stays where the
//!   fast frame put it, less the stack words (aligned down to 16), so every slot a pointer
//!   reaches stays put. `x29` stays the fast frame's: the variant's prologue keeps it.
//! - The callee-saved registers the fast prologue saved are given back their CALLER's values
//!   from the fast frame's save area (blaze-code unwind.rs [`Frame::pairs`]: the `k`th pair at
//!   `x29 - 16 (k + 1)`), after the rows are read from the registers they may sit in. The
//!   variant keeps them as the convention says, so they are the caller's when it returns.
//! - `x30` is [`loop_exit`]: the variant returns into it, and it pops the fast frame (`mov sp,
//!   x29; ldp x29, x30, [sp], #16; ret`), so the value the variant returns reaches the fast
//!   function's caller in the registers it was returned in.
//!
//! SANS-IO: [`on_poll_fault`] reads only `regs` and the record; what needs memory is a list of
//! [`Move`]s in the [`Resume`] it returns, which [`apply`] performs through a [`StackMemory`]:
//! every source read first, then every destination written (one parallel copy). In signal
//! context both run with no allocation and no lock: the list is a fixed array
//! ([`MAX_WORDS`] rows; more is refused by name, never cut).
//!
//! Owed (named, not built here): [`loop_exit`]'s unwind row (a panic unwinding out of the
//! variant passes it: CFA `x29 + 16`, `x29` and `x30` saved at CFA - 16 and - 8), which the
//! unwind registration (unwind_reg.rs) must give it; the signal driver that checks the faulting
//! address is the poll page before it calls here.

use core::fmt;

use blaze_code::unwind::{Frame, reg};
use blaze_tier::record::RecordView;
use blaze_tier::{Place, RegClass};

/// Words passed in registers: `x0..x7`.
pub const ARG_REGS: usize = 8;

/// The most loop-map rows a header may have for its variant to be entered.
pub const MAX_WORDS: usize = 128;

/// The most moves of one resume: a word a row, and the ten callee-saved `x` and eight `d`
/// registers given back.
pub const MAX_MOVES: usize = MAX_WORDS + 18;

/// The bytes of one instruction.
const INST: u64 = 4;

/// A stopped thread's registers on arm64: `x[29]` is the frame pointer and `x[30]` the link
/// register; `d[n]` is the low 64 bits of `v<n>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ThreadRegs {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub d: [u64; 32],
}

/// Where a moved word comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Src {
    /// The word itself (read from a register at the fault, or an address).
    Word(u64),
    /// The 8 bytes at a stack address.
    Stack(u64),
}

/// Where a moved word goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dst {
    /// `x<n>`.
    X(u8),
    /// The low 64 bits of `v<n>`.
    D(u8),
    /// The 8 bytes at a stack address.
    Stack(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Move {
    pub from: Src,
    pub to: Dst,
}

/// How the faulting thread resumes: `regs` already holds the new `pc`, `sp` and `x30`; the
/// moves are what [`apply`] does before the thread runs again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resume {
    /// The variant's entry.
    pub pc: u64,
    pub sp: u64,
    /// [`loop_exit`]'s address.
    pub lr: u64,
    moves: [Move; MAX_MOVES],
    len: usize,
}

impl Resume {
    pub fn moves(&self) -> &[Move] {
        &self.moves[..self.len]
    }

    fn push(&mut self, m: Move) -> Result<(), PollError> {
        let slot = self.moves.get_mut(self.len).ok_or(PollError::TooManyMoves)?;
        *slot = m;
        self.len += 1;
        Ok(())
    }
}

/// The placed code a fault is looked up in.
pub trait PlacedRecords {
    /// The placed record whose code holds `pc`, and the address of its first code byte.
    fn holding(&self, pc: u64) -> Option<(u64, RecordView<'_>)>;
    /// The entry of the placed loop-entry variant of the record at `base` for its loop
    /// `header` (an index into its loops), if one is placed.
    fn variant(&self, base: u64, header: u32) -> Option<u64>;
    /// The address [`loop_exit`] is at in this process.
    fn exit_stub(&self) -> u64;
}

/// The stopped thread's stack, as [`apply`] reads and writes it. The driver's implementation
/// refuses (`None`) any address outside the thread's stack bounds; the tests' is a model.
pub trait StackMemory {
    fn read(&self, at: u64) -> Option<u64>;
    fn write(&mut self, at: u64, word: u64) -> Option<()>;
}

/// Why a fault is not a poll this handler resumes, naming the thing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PollError {
    /// No placed record holds the faulting pc.
    NotPlaced { pc: u64 },
    /// The record holds no poll whose load is at `offset`.
    NotAPoll { offset: u64 },
    /// The record has no frame description: its saved registers cannot be given back.
    NoFrame { offset: u64 },
    /// No variant is placed for the poll's loop (the page was protected for nothing).
    NoVariant { offset: u64, header: u32 },
    /// More loop-map rows than [`MAX_WORDS`].
    TooManyValues { header: u32, count: usize },
    /// More moves than [`MAX_MOVES`].
    TooManyMoves,
    /// A row's register is the frame pointer, the link register or `sp`.
    Register { header: u32, reg: u8 },
    /// A saved pair names a register outside `x19..x28`, `d8..d15`.
    SavedRegister { dwarf: u16 },
    /// The new stack pointer or a save-area address is below 0.
    StackUnderflow { sp: u64 },
    /// The stack refused an address.
    Stack { at: u64 },
}

impl fmt::Display for PollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(backend-next.md D100 points 2 and 3: the poll's fault handler)";
        match *self {
            PollError::NotPlaced { pc } => write!(f, "a poll fault at {pc:#x}, which no placed record holds {rule}"),
            PollError::NotAPoll { offset } => write!(f, "a fault at code offset {offset}, which is no poll's load {rule}"),
            PollError::NoFrame { offset } => {
                write!(f, "the poll at code offset {offset} is in a record with no frame description {rule}")
            }
            PollError::NoVariant { offset, header } => {
                write!(f, "the poll at code offset {offset} faulted, but loop {header} has no variant placed {rule}")
            }
            PollError::TooManyValues { header, count } => {
                write!(f, "loop {header} has {count} live values, more than the {MAX_WORDS} a variant takes {rule}")
            }
            PollError::TooManyMoves => write!(f, "a resume needs more than {MAX_MOVES} moves {rule}"),
            PollError::Register { header, reg } => {
                write!(f, "loop {header}'s map names register {reg}, which holds no value {rule}")
            }
            PollError::SavedRegister { dwarf } => {
                write!(f, "a saved pair names DWARF register {dwarf}, which is not callee-saved {rule}")
            }
            PollError::StackUnderflow { sp } => write!(f, "the variant's stack words do not fit below sp {sp:#x} {rule}"),
            PollError::Stack { at } => write!(f, "the stack refused the word at {at:#x} {rule}"),
        }
    }
}

impl core::error::Error for PollError {}

/// The register a saved pair's DWARF number names.
fn saved_dst(dwarf: u16) -> Result<Dst, PollError> {
    match dwarf {
        19..=28 => Ok(Dst::X(dwarf as u8)),
        d if (reg::V8..reg::V8 + 8).contains(&d) => Ok(Dst::D((d - 64) as u8)),
        _ => Err(PollError::SavedRegister { dwarf }),
    }
}

/// Handles a fault at `pc` on a poll's load: sets `regs`' `pc` (the variant's entry), `sp` and
/// `x30` ([`loop_exit`]) and returns the moves [`apply`] makes (the module's documentation).
/// Nothing is changed in `regs` when it refuses.
pub fn on_poll_fault(pc: u64, regs: &mut ThreadRegs, records: &dyn PlacedRecords) -> Result<Resume, PollError> {
    let (base, record) = records.holding(pc).ok_or(PollError::NotPlaced { pc })?;
    let offset = pc.checked_sub(base).ok_or(PollError::NotPlaced { pc })?;
    // The polls are sorted by `at`; the fault is at the load, one instruction in.
    let load_of = |i: usize| u64::from(record.poll(i).at) + INST;
    let (mut lo, mut hi) = (0usize, record.polls());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if load_of(mid) < offset {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == record.polls() || load_of(lo) != offset {
        return Err(PollError::NotAPoll { offset });
    }
    let poll = record.poll(lo);
    // The seal refuses a poll naming no loop (record/form.rs `PollHeader`).
    let header = record.loop_header(poll.header as usize);
    let frame: Frame = record.unwind_frame().ok_or(PollError::NoFrame { offset })?;
    let entry = records.variant(base, poll.header).ok_or(PollError::NoVariant { offset, header: poll.header })?;
    let rows = record.live_of(header);
    let count = rows.len();
    if count > MAX_WORDS {
        return Err(PollError::TooManyValues { header: poll.header, count });
    }
    let stack_words = count.saturating_sub(ARG_REGS) as u64;
    let sp = regs.sp;
    let new_sp = sp.checked_sub(8 * stack_words).ok_or(PollError::StackUnderflow { sp })? & !15;
    let lr = records.exit_stub();
    let mut resume = Resume {
        pc: entry,
        sp: new_sp,
        lr,
        moves: [Move { from: Src::Word(0), to: Dst::X(0) }; MAX_MOVES],
        len: 0,
    };
    for (i, row) in rows.enumerate() {
        let live = record.live(row);
        let from = match live.place {
            Place::Reg(r) => match r.class() {
                RegClass::Gpr if r.hw() < 29 => Src::Word(regs.x[r.hw() as usize]),
                RegClass::Gpr => return Err(PollError::Register { header: poll.header, reg: r.0 }),
                RegClass::Fpr => Src::Word(regs.d[r.hw() as usize]),
            },
            Place::Cell { sp_offset } => Src::Stack(sp + u64::from(sp_offset)),
            Place::Slot { sp_offset } => Src::Word(sp + u64::from(sp_offset)),
        };
        let to = match i < ARG_REGS {
            true => Dst::X(i as u8),
            false => Dst::Stack(new_sp + 8 * (i - ARG_REGS) as u64),
        };
        resume.push(Move { from, to })?;
    }
    // The caller's callee-saved registers, from the fast frame's save area.
    let fp = regs.x[29];
    for (k, (lo, hi)) in frame.pairs().enumerate() {
        let at = fp.checked_sub(16 * (k as u64 + 1)).ok_or(PollError::StackUnderflow { sp: fp })?;
        resume.push(Move { from: Src::Stack(at), to: saved_dst(lo)? })?;
        resume.push(Move { from: Src::Stack(at + 8), to: saved_dst(hi)? })?;
    }
    regs.pc = entry;
    regs.sp = new_sp;
    regs.x[30] = lr;
    Ok(resume)
}

/// Performs `resume`'s moves on the stopped thread: every source read, then every destination
/// written. Stops at the first address `mem` refuses.
pub fn apply<M: StackMemory>(resume: &Resume, regs: &mut ThreadRegs, mem: &mut M) -> Result<(), PollError> {
    let mut words = [0u64; MAX_MOVES];
    for (i, m) in resume.moves().iter().enumerate() {
        words[i] = match m.from {
            Src::Word(word) => word,
            Src::Stack(at) => mem.read(at).ok_or(PollError::Stack { at })?,
        };
    }
    for (i, m) in resume.moves().iter().enumerate() {
        match m.to {
            Dst::X(n) => regs.x[n as usize] = words[i],
            Dst::D(n) => regs.d[n as usize] = words[i],
            Dst::Stack(at) => mem.write(at, words[i]).ok_or(PollError::Stack { at })?,
        }
    }
    Ok(())
}

/// Where a loop-entry variant returns: pops the fast frame below it and returns to the fast
/// function's caller, every return register as the variant left it. `x29` is the fast frame's
/// (the variant kept it), whose frame record is the top 16 bytes of the fast frame (blaze-emit
/// table_gen/arm64_glue.rs ENTER: `stp x29, x30, [sp, #-16]!; mov x29, sp`).
///
/// # Safety
///
/// Reached only as the return address [`on_poll_fault`] gives a variant, never called: on
/// entry `x29` must be the frame pointer of a fast frame whose callee-saved registers were
/// already given back.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn loop_exit() {
    core::arch::naked_asm!("mov sp, x29", "ldp x29, x30, [sp], #16", "ret")
}
