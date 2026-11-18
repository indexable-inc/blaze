//! A code record's unwind rows (run.md, "End state", "Panics"): what the record's `frame` word
//! and its `pads` mean, written by the emitter (blaze-emit pass/unwind.rs) and read here in
//! place, every row checked, before the launcher turns them into the bytes its platform's
//! unwinder reads ([`crate::eh`]).
//!
//! The rows are platform-neutral: the frame is said as which registers are saved and where,
//! never as Apple's compact encoding or as DWARF instructions; [`crate::eh`] makes DWARF of it,
//! and a compact-unwind writer would make its word of the same rows.
//!
//! # The frame word (`CodeRecord::frame`)
//!
//! `0` is "no description": the record cannot be unwound through, and has no pads. Otherwise
//! the word is a [`Frame`]:
//!
//! | bits | what |
//! |---|---|
//! | 28..32 | the form: `1` is arm64's standard frame record (below); any other is refused |
//! | 0..5 | the pairs `x19:x20`, `x21:x22`, `x23:x24`, `x25:x26`, `x27:x28` saved, one bit each |
//! | 8..12 | the pairs `d8:d9`, `d10:d11`, `d12:d13`, `d14:d15` saved, one bit each |
//! | the rest | zero, or the word is refused |
//!
//! The standard frame is the one blaze-emit's prologue makes (pass.rs `run`): instruction 0 is
//! `stp x29, x30, [sp, #-16]!` and instruction 1 `mov x29, sp`, so from instruction 2 on the
//! canonical frame address (the caller's `sp`) is `x29 + 16`, `x29` is saved at `CFA - 16` and
//! `x30` at `CFA - 8`. Each saved pair is then stored by one `stp` (instruction `2 + k` for the
//! `k`th pair in the order of the table, `x` pairs first) at `x29 - 16 * (k + 1)`, its HIGHER
//! register at the lower address: `stp x20, x19, [sp, #-16]!`, so x19 is at `CFA - 24 - 16k` and
//! x20 at `CFA - 32 - 16k` (and d8 above d9 the same way). This is the platform's one order,
//! which LLVM's prologue writes too and the system's unwinder reads (main's ruling,
//! unwind-P8a-4; there is no second form):
//!
//! - MacOSX27.2.sdk/usr/include/mach-o/compact_unwind_encoding.h:309-315: "UNWIND_ARM64_MODE_FRAME:
//!   This is a standard arm64 prolog where FP/LR are immediately pushed on the stack, then SP is
//!   copied to FP. If there are any non-volatile registers saved, then are copied into the stack
//!   frame in pairs in a contiguous range right below the saved FP/LR pair.  Any subset of the
//!   five X pairs and four D pairs can be saved, but the memory layout must be in register number
//!   order."
//! - libunwind src/CompactUnwinder.hpp:621-631 (the pinned toolchain's llvm-project),
//!   `stepWithCompactEncodingFrame`: "Registers_arm64::reg_t savedRegisterLoc =
//!   registers.getFP() - 8; if (encoding & UNWIND_ARM64_FRAME_X19_X20_PAIR) {
//!   registers.setRegister(UNW_AARCH64_X19, addressSpace.get64(savedRegisterLoc));
//!   savedRegisterLoc -= 8; registers.setRegister(UNW_AARCH64_X20,
//!   addressSpace.get64(savedRegisterLoc)); savedRegisterLoc -= 8; }".
//!
//! So a frame word's pair bits are the compact encoding's `UNWIND_ARM64_FRAME_*_PAIR` bits
//! (x pairs at bits 0..5, d pairs at 8..12) and mean the same layout.
//!
//! The stack size is not in the word: with a frame record the frame address is `x29`-relative,
//! so an unwinder never needs it (the frame may even be realigned below `x29`, as pass.rs does
//! for a slot aligned past 16). A frameless form would carry it; no emitter makes one, so none
//! is defined, and a word naming one is refused.
//!
//! The epilogue is not described: a record is unwound only at its calls (a panic is
//! synchronous), and no call is in a prologue or an epilogue: [`read`] refuses a pad that
//! starts inside the prologue.
//!
//! # A pad (`CodeRecord::pads`)
//!
//! One row per call that may unwind, in the order of their offsets: the call's instruction
//! `[start, start + len)`, the landing block's offset (`landing`), and the [`Action`]. A call
//! with no row cannot unwind: an unwind reaching it ends the program (the personality routine's
//! `EHAction::Terminate`), which is what Rust asks of a call inside a cleanup.

use core::fmt;

use crate::record::{CodeRecord, CodeView, Pad};

/// The form of the frame word: arm64's standard frame record.
pub const FORM_FRAME: u32 = 1;
const FORM_SHIFT: u32 = 28;
const X_PAIRS: u32 = 0x1f;
const D_SHIFT: u32 = 8;
const D_PAIRS: u32 = 0xf;
/// Every bit a [`FORM_FRAME`] word may set.
const FRAME_BITS: u32 = (0xf << FORM_SHIFT) | X_PAIRS | (D_PAIRS << D_SHIFT);

/// An arm64 instruction's bytes: every offset here is a multiple.
pub const INST: u32 = 4;

/// The DWARF register numbers of arm64 (DWARF for the Arm 64-bit Architecture, "DWARF register
/// names"): `x0..x30` are `0..30`, `sp` is 31, `v0..v31` (whose low half is `d0..d31`) are
/// `64..95`.
pub mod reg {
    pub const FP: u16 = 29;
    pub const LR: u16 = 30;
    pub const SP: u16 = 31;
    pub const X19: u16 = 19;
    pub const V8: u16 = 72;
}

/// A function's frame, as its prologue makes it (the module's doc says the layout).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame {
    /// Bit `k`: the pair `x(19 + 2k):x(20 + 2k)` is saved.
    pub x_pairs: u8,
    /// Bit `k`: the pair `d(8 + 2k):d(9 + 2k)` is saved.
    pub d_pairs: u8,
}

impl Frame {
    /// The frame record and nothing else saved: what blaze-emit's prologue makes today.
    pub const PLAIN: Frame = Frame { x_pairs: 0, d_pairs: 0 };

    /// The frame word, or the field past its bits named.
    pub fn word(self) -> Result<u32, UnwindError> {
        if u32::from(self.x_pairs) & !X_PAIRS != 0 || u32::from(self.d_pairs) & !D_PAIRS != 0 {
            return Err(UnwindError::FrameBits { word: (u32::from(self.d_pairs) << D_SHIFT) | u32::from(self.x_pairs) });
        }
        Ok((FORM_FRAME << FORM_SHIFT) | (u32::from(self.d_pairs) << D_SHIFT) | u32::from(self.x_pairs))
    }

    /// The frame a word names; `None` for the word `0` (no description).
    pub fn of_word(word: u32) -> Result<Option<Frame>, UnwindError> {
        if word == 0 {
            return Ok(None);
        }
        let form = word >> FORM_SHIFT;
        if form != FORM_FRAME {
            return Err(UnwindError::FrameForm { word, form });
        }
        if word & !FRAME_BITS != 0 {
            return Err(UnwindError::FrameBits { word });
        }
        Ok(Some(Frame { x_pairs: (word & X_PAIRS) as u8, d_pairs: ((word >> D_SHIFT) & D_PAIRS) as u8 }))
    }

    /// The saved pairs, in the order their `stp`s come, each as its `stp` names them:
    /// `(the register at the lower address, the one at the higher)` as DWARF numbers, the `k`th
    /// at `CFA - 32 - 16k` and `CFA - 24 - 16k`. That is `(x20, x19)`: the platform's order
    /// (module doc, "The frame word").
    pub fn pairs(self) -> impl Iterator<Item = (u16, u16)> {
        let x = (0..5u16).filter(move |k| self.x_pairs & (1 << k) != 0).map(|k| (reg::X19 + 2 * k + 1, reg::X19 + 2 * k));
        let d = (0..4u16).filter(move |k| self.d_pairs & (1 << k) != 0).map(|k| (reg::V8 + 2 * k + 1, reg::V8 + 2 * k));
        x.chain(d)
    }

    /// The prologue's bytes: the frame record's two instructions and one `stp` a saved pair.
    /// No call is before this offset.
    pub fn prologue(self) -> u32 {
        INST * (2 + self.x_pairs.count_ones() + self.d_pairs.count_ones())
    }
}

/// What the personality routine does when an unwind reaches a call (the Itanium C++ ABI's
/// call-site record and action, as the pinned library's `find_eh_action` reads them).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// No landing: unwinding goes on to the caller (`cs_lpad == 0`: `EHAction::None`).
    Continue,
    /// Land, drop what is live, resume (`cs_action_entry == 0`: `EHAction::Cleanup`).
    Cleanup,
    /// Land and stop unwinding: `catch_unwind`'s intrinsic (an action record whose type index
    /// is positive: `EHAction::Catch`; the type is the catch-all, a null entry).
    Catch,
    /// Land and end the program: a call that must not unwind (a negative type index, an empty
    /// filter: `EHAction::Filter`).
    Terminate,
}

impl Action {
    pub const fn code(self) -> u32 {
        match self {
            Action::Continue => 0,
            Action::Cleanup => 1,
            Action::Catch => 2,
            Action::Terminate => 3,
        }
    }

    pub const fn of_code(code: u32) -> Option<Action> {
        match code {
            0 => Some(Action::Continue),
            1 => Some(Action::Cleanup),
            2 => Some(Action::Catch),
            3 => Some(Action::Terminate),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Action::Continue => "continue",
            Action::Cleanup => "cleanup",
            Action::Catch => "catch",
            Action::Terminate => "terminate",
        }
    }
}

/// One call that may unwind: `[start, start + len)` holds its call instruction; an unwind
/// through it lands at `landing` (0 for [`Action::Continue`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub start: u32,
    pub len: u32,
    pub landing: u32,
    pub action: Action,
}

impl Row {
    /// The record's row for it.
    pub const fn pad(self) -> Pad {
        Pad { start: self.start, len: self.len, landing: self.landing, action: self.action.code() }
    }
}

/// Why a record's unwind rows were refused: the first broken rule, naming the word or the
/// offset it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnwindError {
    /// The frame word names a form this reader does not know.
    FrameForm { word: u32, form: u32 },
    /// The frame word sets a bit its form does not use.
    FrameBits { word: u32 },
    /// Pads on a record with no frame description: nothing could find them.
    PadsWithoutFrame { pads: u32 },
    /// A pad's action is not one of [`Action`]'s codes.
    PadAction { start: u32, action: u32 },
    /// A pad covers no instruction, or is not on instruction boundaries.
    PadShape { start: u32, len: u32 },
    /// A pad starts inside the prologue, where the frame is not yet the body's.
    PadInPrologue { start: u32, prologue: u32 },
    /// A pad runs past the code.
    PadOutside { start: u32, len: u32, code: u32 },
    /// A pad starts before the previous one ended (the rows are sorted and disjoint).
    PadsOverlap { start: u32 },
    /// A landing is 0 while the action lands, or not 0 while it does not, or is not an
    /// instruction of the code past the prologue.
    PadLanding { start: u32, landing: u32, action: Action },
}

impl fmt::Display for UnwindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-code unwind.rs, \"The frame word\", \"A pad\")";
        match *self {
            UnwindError::FrameForm { word, form } => write!(f, "a code record's frame word {word:#x} names form {form}, which is not arm64's frame record (1) {rule}"),
            UnwindError::FrameBits { word } => write!(f, "a code record's frame word {word:#x} sets bits its form does not use {rule}"),
            UnwindError::PadsWithoutFrame { pads } => write!(f, "a code record has {pads} landing pads and no frame description {rule}"),
            UnwindError::PadAction { start, action } => write!(f, "a code record's pad at {start} has the unknown action {action} {rule}"),
            UnwindError::PadShape { start, len } => write!(f, "a code record's pad at {start}, {len} bytes, is not a run of whole instructions {rule}"),
            UnwindError::PadInPrologue { start, prologue } => write!(f, "a code record's pad at {start} is inside its {prologue}-byte prologue {rule}"),
            UnwindError::PadOutside { start, len, code } => write!(f, "a code record's pad at {start}, {len} bytes, runs past its {code} bytes of code {rule}"),
            UnwindError::PadsOverlap { start } => write!(f, "a code record's pad at {start} overlaps the pad before it {rule}"),
            UnwindError::PadLanding { start, landing, action } => {
                write!(f, "a code record's pad at {start} ({}) lands at {landing}, which that action does not allow {rule}", action.name())
            }
        }
    }
}

impl core::error::Error for UnwindError {}

/// Writes `frame` and `rows` into `record`: the frame word and one pad a row. The rows are
/// checked when the record is read ([`read`]); `CodeRecord::seal` sorts them.
pub fn fill(record: &mut CodeRecord<'_>, frame: Frame, rows: &[Row]) -> Result<(), UnwindError> {
    record.frame = frame.word()?;
    for row in rows {
        record.pads.push(row.pad());
    }
    Ok(())
}

/// A record's unwind rows, read in place from its encoded form ([`read`]) or taken from rows in
/// memory ([`of_rows`]: blaze-tier's record), and checked whole either way.
#[derive(Clone, Copy)]
pub struct Unwind<'b> {
    frame: Option<Frame>,
    rows: RowsIn<'b>,
}

/// Where an [`Unwind`]'s rows are.
#[derive(Clone, Copy)]
enum RowsIn<'b> {
    /// A code record's pads (record.rs, `bzcode04`).
    View(CodeView<'b>),
    /// Rows in memory, for `code_len` bytes of code.
    Rows { rows: &'b [Row], code_len: u32 },
}

/// Reads `view`'s unwind rows, checking every one: the frame word's form and bits; each pad's
/// action, its shape, that it lies past the prologue and inside the code, that the pads are
/// sorted and disjoint, and that each landing is where its action says.
pub fn read(view: CodeView<'_>) -> Result<Unwind<'_>, UnwindError> {
    let frame = Frame::of_word(view.frame())?;
    check(frame, view.pads(), view.code().len() as u64, |i| view.pad(i))?;
    Ok(Unwind { frame, rows: RowsIn::View(view) })
}

/// The same [`Unwind`] as [`read`] gives, from a frame and rows in memory for `code_len` bytes
/// of code (blaze-tier's `Emitted::unwind_frame` and `Emitted::unwind`), checked by the same
/// rules: a record made by any tier is unwound through by the one entry writer
/// ([`crate::eh::entry`]).
pub fn of_rows(frame: Option<Frame>, rows: &[Row], code_len: u32) -> Result<Unwind<'_>, UnwindError> {
    check(frame, rows.len(), u64::from(code_len), |i| rows[i].pad())?;
    Ok(Unwind { frame, rows: RowsIn::Rows { rows, code_len } })
}

/// [`read`]'s rules over `pads` pads of `code` bytes of code, pad `i` given by `pad(i)`.
fn check(frame: Option<Frame>, pads: usize, code: u64, pad: impl Fn(usize) -> Pad) -> Result<(), UnwindError> {
    let Some(frame) = frame else {
        return match pads {
            0 => Ok(()),
            n => Err(UnwindError::PadsWithoutFrame { pads: n as u32 }),
        };
    };
    let prologue = frame.prologue();
    let mut end = 0u64;
    for i in 0..pads {
        let pad = pad(i);
        let action = Action::of_code(pad.action).ok_or(UnwindError::PadAction { start: pad.start, action: pad.action })?;
        if pad.len == 0 || pad.start % INST != 0 || pad.len % INST != 0 {
            return Err(UnwindError::PadShape { start: pad.start, len: pad.len });
        }
        if pad.start < prologue {
            return Err(UnwindError::PadInPrologue { start: pad.start, prologue });
        }
        let past = u64::from(pad.start) + u64::from(pad.len);
        if past > code {
            return Err(UnwindError::PadOutside { start: pad.start, len: pad.len, code: code as u32 });
        }
        if u64::from(pad.start) < end {
            return Err(UnwindError::PadsOverlap { start: pad.start });
        }
        end = past;
        let lands = action != Action::Continue;
        let landing_ok = match lands {
            false => pad.landing == 0,
            true => pad.landing >= prologue && pad.landing % INST == 0 && u64::from(pad.landing) < code,
        };
        if !landing_ok {
            return Err(UnwindError::PadLanding { start: pad.start, landing: pad.landing, action });
        }
    }
    Ok(())
}

impl<'b> Unwind<'b> {
    /// The frame, `None` when the record has no description (and so no pads).
    pub fn frame(&self) -> Option<Frame> {
        self.frame
    }

    /// The code's bytes: the range the unwind entry covers.
    pub fn code_len(&self) -> u32 {
        match self.rows {
            RowsIn::View(view) => view.code().len() as u32,
            RowsIn::Rows { code_len, .. } => code_len,
        }
    }

    pub fn rows(&self) -> usize {
        match self.rows {
            RowsIn::View(view) => view.pads(),
            RowsIn::Rows { rows, .. } => rows.len(),
        }
    }

    /// Row `i`, already checked by [`read`] or [`of_rows`].
    pub fn row(&self, i: usize) -> Row {
        let pad = match self.rows {
            RowsIn::View(view) => view.pad(i),
            RowsIn::Rows { rows, .. } => return rows[i],
        };
        let action = match Action::of_code(pad.action) {
            Some(action) => action,
            None => unreachable!("`read` refused an unknown action"),
        };
        Row { start: pad.start, len: pad.len, landing: pad.landing, action }
    }

    /// Whether any row lands: only then does the entry need a personality routine and a
    /// language-specific data area.
    pub fn lands(&self) -> bool {
        (0..self.rows()).any(|i| self.row(i).action != Action::Continue)
    }
}
