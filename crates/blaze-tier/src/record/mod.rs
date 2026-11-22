//! What every tier emits: the ONE new code record (D100 point 11), in memory. Its on-disk form
//! (encode, read, seal, place) is the record-format piece's, written in this crate beside these
//! rows (record/form.rs); blaze-code's `bzcode01` and its paired sites are retired when the last
//! user has moved, and blaze-code keeps its leaf types (`Symbol`, `Space`, `Target`, `Word`,
//! `Pad`), its unwind rows (`unwind::{Frame, Row}`), segments and the image.
//!
//! New against `bzcode01`: sites at RELOCATION grain ([`Reloc`]: today's Load is two relocations
//! and Call three, so LLVM's code is read back whatever its scheduler did); the LOOP MAP
//! ([`LoopHeader`], [`Live`]: D98, homes fixed at each header); the POLLS ([`Poll`]: D100 point 3,
//! a Load-site pair through x16, no reserved register); the unwind rows of every tier (point
//! 10); the POSITION MAP with its inline frames ([`Position`], [`InlineFrame`]: point 10); the
//! READ LIST (blaze-run's [`Read`] rows: the simplifier's spliced bodies at Fast, exactly the
//! inlined set and the fact records of callees still called at Optimized, point 7).
//!
//! New in `bzcode03` (D106): the DECLARED INSTANCES ([`DeclaredRow`], declared.rs): for each
//! in-program target the data its record's key is made from without the body, so `blaze run`
//! follows a found record to what it calls. A producer fills them with [`declare`] before the
//! seal; the tiers' emitters leave them empty (their caller holds the names).
//!
//! New in `bzcode08` (the columns blaze-code's `bzcode07` has and this form lacked, so one form
//! serves the run path: D195): the TRAP ROWS (blaze-code `TrapRow`, with their held values and
//! subjects), each part's PLACEMENT ALIGNMENT (`code_align`, `relro_align`) and the
//! SOURCE-POSITION SLOTS (blaze-code `PositionRow`, filled at placement). The number skips
//! `04`..`07`, blaze-code's, so one number never names two layouts and a `bzcode07` record read
//! here is refused as an older form.
//!
//! Positions are item-relative (rule 71): code offsets from the record's first code byte, spans
//! as byte ranges in the item's text, values by LIR index. Sorted at the boundary (rule 25), as
//! each row's documentation says; the seal refuses an unsorted list by name.

use blaze_code::record::{LEAST_CODE_ALIGN, LEAST_RELRO_ALIGN, Pad, PositionRow, Symbol, Target, TrapRow, Word};
use blaze_code::unwind::{Frame, Row};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::InstanceKey;
use blaze_run::parent::Read;

use crate::Reg;

mod declared;
mod form;
mod place;
mod v1;

pub use declared::{Declare, Declared, DeclaredKind, DeclaredRow, Declares, declare};
pub use form::{FormError, MAGIC, RecordView, Sealed, encode, read, seal};
pub use place::{calls, page_offset, place, place_resolved};
pub use v1::from_v1;

#[cfg(test)]
mod declared_tests;
#[cfg(test)]
mod form_tests;

/// A relocation's kind: what instruction field or data word it completes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum RelocKind {
    /// arm64 `adrp`: the target's 4 KiB page, pc-relative, 21 bits.
    Page21 = 0,
    /// arm64 low 12 bits of the target in an `add` (scale 0) or a load or store of `1 << scale`
    /// bytes.
    PageOff12 { scale: u8 } = 1,
    /// arm64 `b`/`bl`: 26-bit word displacement.
    Branch26 = 2,
    /// arm64 `adrp` of the target's import word (its GOT entry).
    GotPage21 = 3,
    /// arm64 `ldr` of the target's import word, low 12 bits scaled by 8.
    GotPageOff12 = 4,
    /// A 64-bit absolute address in relro.
    Abs64 = 5,
    /// x86-64 32-bit pc-relative (a `call`, `jmp` or `lea`).
    Pc32 = 6,
    /// x86-64 32-bit pc-relative to the target's import word.
    GotPc32 = 7,
}

/// One relocation: sorted by `at`, at most one a byte.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Reloc {
    /// The completed field's instruction (or relro word) offset.
    pub at: u32,
    /// In `code` or in `relro`.
    pub in_relro: bool,
    pub kind: RelocKind,
    /// An index into the record's targets, or blaze-code's `OWN_CODE` / `OWN_RELRO`.
    pub target: u32,
    pub addend: i64,
}

/// One loop header of the emitted code: sorted by `at`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LoopHeader {
    /// The header's first code byte.
    pub at: u32,
    /// Its live values: `live[first..first + count]`, sorted by value.
    pub first: u32,
    pub count: u32,
}

/// Where a value lives for the whole loop: its fixed place at the header.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Place {
    /// Its 16-byte cell at `sp + sp_offset`.
    Cell { sp_offset: u32 },
    /// A register.
    Reg(Reg),
    /// A stack slot whose address is taken: it stays in the fast frame and reaches the
    /// loop-entry variant as a pointer (D100 point 2).
    Slot { sp_offset: u32 },
}

/// One value live into a loop header.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Live {
    /// The row's key: the value's LIR index; for [`Place::Slot`], the function's instruction
    /// count plus the slot's index (one key space, so a header lists each key once).
    pub value: u32,
    pub place: Place,
}

/// One poll on a loop's back edge: `adrp x16, poll@PAGE; ldr xzr, [x16, poll@PAGEOFF]`, whose two
/// relocations name the runtime's poll page. Sorted by `at`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Poll {
    /// The poll's first instruction.
    pub at: u32,
    /// Its loop: an index into [`Emitted::loops`].
    pub header: u32,
}

/// No parent inline frame: the function's own body.
pub const OUTERMOST: u32 = u32::MAX;

/// One inline frame the simplifier or LLVM made: a body spliced at a call site.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct InlineFrame {
    /// The frame it was spliced into: an index into the frames, or [`OUTERMOST`]. Parents come
    /// before children.
    pub parent: u32,
    pub instance: InstanceKey,
    /// The call site's span in the parent's item text.
    pub call_lo: u32,
    pub call_hi: u32,
}

/// One row of the position map: from `at` up to the next row's `at`, the code is of `frame`'s
/// body at that span. Sorted by `at`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Position {
    pub at: u32,
    /// An index into the inline frames, or [`OUTERMOST`].
    pub frame: u32,
    pub span_lo: u32,
    pub span_hi: u32,
}

/// One function as a tier emitted it: the new code record in memory.
pub struct Emitted<'a> {
    pub code: AVec<'a, u8>,
    pub relro: AVec<'a, u8>,
    pub targets: AVec<'a, Target>,
    pub relocs: AVec<'a, Reloc>,
    pub words: AVec<'a, Word>,
    /// The frame's size in bytes below the frame record.
    pub frame: u32,
    pub pads: AVec<'a, Pad>,
    /// The unwind frame and rows (blaze-code unwind.rs), for every tier.
    pub unwind_frame: Option<Frame>,
    pub unwind: AVec<'a, Row>,
    pub loops: AVec<'a, LoopHeader>,
    pub live: AVec<'a, Live>,
    pub polls: AVec<'a, Poll>,
    pub frames: AVec<'a, InlineFrame>,
    pub positions: AVec<'a, Position>,
    /// Sorted by `fact`, unique.
    pub reads: AVec<'a, Read>,
    /// Each C name a target uses: its symbol and its text's range in `text`.
    pub names: AVec<'a, (Symbol, u32, u32)>,
    /// Each in-program target's key data (declared.rs): its name's range in `text`. The seal
    /// sorts them by symbol and refuses any that disagree with the targets.
    pub declared: AVec<'a, DeclaredRow>,
    pub text: AVec<'a, u8>,
    /// log2 of the alignment the code's first byte is placed at: the most the code's own data
    /// rows ask (each row's offset in `code` a multiple of its alignment), from
    /// [`LEAST_CODE_ALIGN`] to blaze-code's `MOST_PART_ALIGN`. The placer refuses an address
    /// that does not honour it (place.rs).
    pub code_align: u8,
    /// log2 of the alignment the relro part's first byte is placed at, as `code_align` for its
    /// rows; at least [`LEAST_RELRO_ALIGN`].
    pub relro_align: u8,
    /// Per `unreachable`'s `brk`, the construct it ends (blaze-code record.rs `TrapRow`: its
    /// item-relative site, where the switched value is held, the enum read): what the run's
    /// fault report names. In any order; the seal sorts them by offset.
    pub traps: AVec<'a, TrapRow>,
    /// Each four-byte SOURCE-POSITION SLOT of the code's or relro part's own data (blaze-code
    /// record.rs `PositionRow`: `line!()`/`column!()` of an item-relative site, rule 71): zero
    /// in the record, filled by the placer with the run's answer (place.rs `place_resolved`),
    /// so no line or column is in a record's bytes or key. In any order; the seal sorts them by
    /// (part, offset).
    pub position_slots: AVec<'a, PositionRow>,
}

impl<'a> Emitted<'a> {
    /// An empty record in `alloc`.
    pub fn new_in(alloc: Alloc<'a>) -> Emitted<'a> {
        Emitted {
            code: Vec::new_in(alloc),
            relro: Vec::new_in(alloc),
            targets: Vec::new_in(alloc),
            relocs: Vec::new_in(alloc),
            words: Vec::new_in(alloc),
            frame: 0,
            pads: Vec::new_in(alloc),
            unwind_frame: None,
            unwind: Vec::new_in(alloc),
            loops: Vec::new_in(alloc),
            live: Vec::new_in(alloc),
            polls: Vec::new_in(alloc),
            frames: Vec::new_in(alloc),
            positions: Vec::new_in(alloc),
            reads: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            declared: Vec::new_in(alloc),
            text: Vec::new_in(alloc),
            code_align: LEAST_CODE_ALIGN,
            relro_align: LEAST_RELRO_ALIGN,
            traps: Vec::new_in(alloc),
            position_slots: Vec::new_in(alloc),
        }
    }

    /// Header `header`'s live values; `None` when its range is past `live` (a malformed map,
    /// which the seal refuses by name).
    pub fn live_of(&self, header: LoopHeader) -> Option<&[Live]> {
        let from = header.first as usize;
        let to = from.checked_add(header.count as usize)?;
        self.live.get(from..to)
    }
}
