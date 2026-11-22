//! Reach: every function whose instructions stay below `b`'s ±128 MiB is emitted, however far
//! its branches and PC-relative references go (rule 3: no other backend takes it; rule 4).
//!
//! # The reaches
//!
//! | form | field | reach | what this pass makes of it |
//! |---|---|---|---|
//! | `b` | 26-bit words | ±128 MiB | every unconditional branch; the bound of a function |
//! | `b.cond`, `cbz`, `cbnz` | 19-bit words | ±1 MiB | a switch case, a `br`'s taken edge |
//! | `ldr xt, <literal>` | 19-bit words | ±1 MiB | the literal-word form's words after the code |
//! | `adr` | 21-bit bytes | ±1 MiB | the function's own data after the code |
//! | `tbz`, `tbnz` | 14-bit words | ±32 KiB | none to a label: the pass makes none |
//!
//! The record form's import and relro sites are `adrp` pairs (±4 GiB), placed by the segment's
//! layout (pass/sites.rs); the branches inside one instruction's code (`land`) stay within it.
//!
//! # The design: short first, islands for what is not placed yet
//!
//! - A conditional branch to a block already placed (a back edge) knows its distance when it is
//!   emitted: past 1 MiB it is emitted long at once, the inverted test over a `b` (`b.ne +8; b L`
//!   for `b.eq L`, `cbz x9, +8; b L` for `cbnz x9, L`): [`Pass::branch_if`].
//! - A short field whose target is not placed yet (a later block; a literal word or a data row,
//!   all after the code) is PENDING, in emission order, which is the order of its deadline (the
//!   last byte it reaches: its offset plus [`SHORT_REACH`]).
//! - At each SAFE POINT (before each block, each instruction, each switch case: no
//!   instruction's own patched span is open there) the pass compares the oldest unsettled
//!   pending field's deadline with where an island placed now would end, plus [`MARGIN`] for
//!   the code until the next safe point. When it would not fit, an ISLAND goes here: `b over`,
//!   then, for each pending field due before the next island could come (twice the margin), in
//!   emission order: a branch veneer `b L` the field is pointed at; for own data, a veneer
//!   `adr x9, #0; movz x10, #lo; movk x10, #hi, lsl #16; add x9, x9, x10; b back` the use
//!   site's `adr` becomes `b` to; for a literal word (the literal-word form only), a copy of the
//!   word, 8-aligned, the `ldr` is pointed at (one copy per word per island); then `over:`.
//! - After the last instruction and the stack-probe tail, one last island (nothing falls into
//!   it) covers the fields to the literal words and the data after the code, by a bound of
//!   their sizes ([`Pass::reach_end`]).
//! - Own data larger than [`MARGIN`] in all is reached by the far form at every use, in line:
//!   `adr x9, #0; movz x10; movk x10; add x9, x9, x10` ([`Field::Far32`]).
//!
//! Why islands and not relaxation by insertion: every table the pass keeps holds an offset
//! written as it is emitted (the unwind rows' calls, the trap rows, the import and relro
//! sites, the `byval` and `sp` fixes, the frame's two words and its probe tail, the branches
//! `land` patched inside one instruction's code). Inserting a word after the fact moves every
//! one of them; an island never moves a byte already placed, so every offset is final when it
//! is written, no table is remapped, and the pass stays one forward walk with no fixpoint.
//!
//! # The bounds (rule 18)
//!
//! One walk, no iteration. Each island settles at least one pending field (the oldest, which
//! made it), so there are at most as many islands as short fields, and each clears every field
//! due within `2 * MARGIN`, so in straight code one island per [`MARGIN`] of code at most. A
//! veneer of the oldest field is placed before the veneers of any field newer than the last
//! safe point, so its distance is at most the margin past the previous check; `settle` checks
//! every field's reach anyway and refuses one past it by name (`TooLarge`): an instruction
//! whose own code passes [`MARGIN`] between two safe points (a jump with tens of thousands of
//! block arguments) is refused, never emitted wrong.
//!
//! # Deterministic (rule 25)
//!
//! Where an island goes and what it holds depend only on the code before it; its entries are in
//! emission order.
//!
//! # The floor (rule 67)
//!
//! Relaxation's floor is 4 bytes per far conditional branch. An island costs its `b over` (4
//! bytes per island) and a veneer per field due (4 bytes a branch; 20 own data; 8 to 12 a word's
//! copy), and a far branch through a veneer runs one more `b`. A function below 1 MiB less the
//! margins places no island and no far form: its bytes are the short-only pass's
//! (reach_tests.rs `a_small_function_is_byte_equal`).

use blaze_lir::View;
use blaze_lir::func::NONE;
use blaze_own::{AVec, Alloc};

use super::{Label, Pass, Patch};
use crate::{
    EmitError, Fixup, Reloc,
    enc::{self, Field, Reg, X9, X10},
    enc_probe::NOP,
};

/// What the pass does for a target past a short field's reach.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Reach {
    /// Islands and long forms: every function whose instructions stay below 128 MiB.
    Islands,
    /// The short forms only: a target past ±1 MiB is refused in `settle` (the control of the
    /// reach tests: the pass as it was before islands).
    #[cfg_attr(not(all(test, target_arch = "aarch64")), expect(dead_code, reason = "the reach tests' control (reach_tests.rs); no build path takes it"))]
    ShortOnly,
}

/// The farthest forward a 19-bit word field (`b.cond`, `cbz`, `cbnz`, `ldr` literal) reaches
/// from its instruction, in bytes. `adr`'s byte field reaches 3 bytes more and is held to this.
pub(super) const SHORT_REACH: u64 = (1 << 20) - 4;

/// The most code the pass lowers between two safe points with an island still placeable in
/// time: an island is placed while the oldest pending field has this much reach left past the
/// island's largest size. A quarter of the short reach: a function's islands cost at most 4
/// bytes per 256 KiB besides their veneers.
pub(crate) const MARGIN: u64 = 1 << 18;

/// The most bytes a `b` reaches forward from the function's first instruction: past it the
/// function is refused by name.
const LONG_REACH: u64 = 1 << 27;

/// An island's own words: its `b over`.
const ISLAND_HEAD: u64 = 4;

/// The pending short fields and what the islands placed.
pub(crate) struct Far<'s> {
    pub(super) reach: Reach,
    scratch: Alloc<'s>,
    /// Indices into the pass's patches of the short fields whose target was not placed when
    /// they were emitted, in emission order (so in deadline order).
    pending: AVec<'s, u32>,
    /// The first entry of `pending` an island or a placed block has not settled.
    head: usize,
    /// The most bytes the unsettled entries' veneers take (an overestimate: an entry settled by
    /// its block is counted until it reaches the head).
    owed: u64,
    /// The literal words' copies placed in islands (the literal-word form), appended to the
    /// fixups after the pool's own.
    pub(super) fixups: AVec<'s, Fixup>,
    /// The most bytes the function's own data takes after the code: each row and 15 bytes of
    /// alignment.
    data_bound: u64,
}

impl<'s> Far<'s> {
    pub(crate) fn new(reach: Reach, view: View<'_>, scratch: Alloc<'s>) -> Far<'s> {
        let data_bound = view.datas.iter().map(|rec| u64::from(rec.bytes_len) + 15).sum();
        Far { reach, scratch, pending: AVec::new_in(scratch), head: 0, owed: 0, fixups: AVec::new_in(scratch), data_bound }
    }
}

/// The last byte a short field at `at` reaches forward.
const fn deadline(at: u32) -> u64 {
    at as u64 + SHORT_REACH
}

/// The most bytes an island takes for an entry reaching `to`.
const fn owed_of(to: Label) -> u64 {
    match to {
        Label::Block(_) => 4,
        // `adr; movz; movk; add; b`.
        Label::Data(_) => 20,
        // The word, and a `nop` before it to align it.
        Label::Literal(_) | Label::Word(_) => 12,
        Label::At(_) => 0,
    }
}

impl<A: core::alloc::Allocator> Pass<'_, '_, A> {
    /// Whether the entry for patch `index` needs no island: its block is placed (in reach: the
    /// safe points saw to it), or an island already pointed it at a veneer.
    fn settled(&self, index: u32) -> bool {
        match self.patches[index as usize].to {
            Label::Block(block) => self.block_at[block as usize] != NONE,
            Label::At(_) => true,
            Label::Literal(_) | Label::Word(_) | Label::Data(_) => false,
        }
    }

    /// `put_to`'s note of a short field whose target is not placed yet.
    pub(super) fn note_pending(&mut self, index: u32, field: Field, to: Label) {
        if self.far.reach == Reach::ShortOnly || !matches!(field, Field::Imm19 | Field::Adr) {
            return;
        }
        let placed = match to {
            Label::Block(block) => self.block_at[block as usize] != NONE,
            Label::At(_) => true,
            Label::Literal(_) | Label::Word(_) | Label::Data(_) => false,
        };
        if !placed {
            self.far.pending.push(index);
            self.far.owed += owed_of(to);
        }
    }

    /// A conditional branch to block `to`: `short` (its 19-bit field zero) when `to` is not
    /// placed yet (an island reaches it if it ends up far) or is placed within 1 MiB back; else
    /// `over`, the inverted test branching 8 bytes on, then `b to`.
    pub(super) fn branch_if(&mut self, short: u32, over: u32, to: u32) {
        let placed = self.block_at[to as usize];
        let far = self.far.reach == Reach::Islands && placed != NONE && u64::from(self.here() - placed) > 1 << 20;
        match far {
            true => {
                self.put(over);
                self.put_to(enc::b(0), Field::Imm26, Label::Block(to));
            }
            false => self.put_to(short, Field::Imm19, Label::Block(to)),
        }
    }

    /// `x9 = ` the address of the data row placed `index`th (after the code): `adr x9`, or the
    /// far form when the function's own data alone may pass `adr`'s reach.
    pub(super) fn own_data_addr(&mut self, index: u32) {
        match self.far.reach == Reach::Islands && self.far.data_bound >= MARGIN {
            true => {
                self.patches.push(Patch { at: self.here(), field: Field::Far32, to: Label::Data(index) });
                self.far_adr(X9);
            }
            false => self.put_to(enc::adr(X9, 0), Field::Adr, Label::Data(index)),
        }
    }

    /// The far own-data form's four words, its offset zero until `settle` (the patch is the
    /// caller's): `adr rd, #0; movz x10, #0; movk x10, #0, lsl #16; add rd, rd, x10`.
    fn far_adr(&mut self, rd: Reg) {
        self.put(enc::adr(rd, 0));
        self.put(enc::movz(X10, 0, 0));
        self.put(enc::movk(X10, 0, 1));
        self.put(enc::add(rd, rd, X10));
    }

    /// A safe point: places an island here when the oldest unsettled short field's reach would
    /// lapse before the next safe point could place one.
    pub(super) fn reach_point(&mut self) -> Result<(), EmitError> {
        if self.far.reach == Reach::ShortOnly {
            return Ok(());
        }
        while self.far.head < self.far.pending.len() && self.settled(self.far.pending[self.far.head]) {
            let to = self.patches[self.far.pending[self.far.head] as usize].to;
            self.far.owed = self.far.owed.saturating_sub(owed_of(to));
            self.far.head += 1;
        }
        let Some(&first) = self.far.pending.get(self.far.head) else { return Ok(()) };
        let most = ISLAND_HEAD + self.far.owed;
        let here = u64::from(self.here());
        if here + most + MARGIN <= deadline(self.patches[first as usize].at) {
            return Ok(());
        }
        self.island(here + most + 2 * MARGIN, true)
    }

    /// After the last instruction and the probe tail (`finish`): the last island, for the
    /// fields to the literal words and the data after the code that may lie past their reach.
    /// Refuses a function whose instructions pass a `b`'s reach.
    pub(super) fn reach_end(&mut self) -> Result<(), EmitError> {
        let here = u64::from(self.here());
        if here >= LONG_REACH {
            return Err(EmitError::TooLarge { what: "a function of 128 MiB of instructions or more (past a `b`'s reach)" });
        }
        if self.far.reach == Reach::ShortOnly {
            return Ok(());
        }
        // Where the last byte after the code may be: this island at its largest, the pool's
        // 8-alignment and its words, the data rows with their alignment.
        let pool = 8 * (self.fixups.len() + self.words.len()) as u64;
        let bound = here + ISLAND_HEAD + self.far.owed + 8 + pool + self.far.data_bound + 1;
        self.island(bound, false)
    }

    /// Places an island here for every pending field whose deadline is before `bound` and that
    /// is not settled, in emission order; nothing when there is none. `fallthrough`: the code
    /// before may run into it, so it begins with `b over`.
    fn island(&mut self, bound: u64, fallthrough: bool) -> Result<(), EmitError> {
        let due = self.far.pending[self.far.head..]
            .iter()
            .take_while(|&&index| deadline(self.patches[index as usize].at) < bound)
            .any(|&index| !self.settled(index));
        if !due {
            return Ok(());
        }
        let too_far = EmitError::TooLarge { what: "a branch or PC-relative reference past its reach" };
        let over = self.here();
        if fallthrough {
            self.put(enc::b(0));
        }
        // The literal words copied into this island: (what, where), one copy each.
        let mut copies: AVec<'_, (Label, u32)> = AVec::new_in(self.far.scratch);
        while self.far.head < self.far.pending.len() {
            let index = self.far.pending[self.far.head];
            let patch = self.patches[index as usize];
            if deadline(patch.at) >= bound {
                break;
            }
            self.far.head += 1;
            self.far.owed = self.far.owed.saturating_sub(owed_of(patch.to));
            if self.settled(index) {
                continue;
            }
            match patch.to {
                Label::Block(block) => {
                    let veneer = self.here();
                    self.put_to(enc::b(0), Field::Imm26, Label::Block(block));
                    self.patches[index as usize].to = Label::At(veneer);
                }
                Label::Data(row) => {
                    // The use site's `adr rd` becomes `b veneer`; the veneer makes the address
                    // in `rd` and branches back to the instruction after the use site.
                    let rd = self.word_at(patch.at) & 31;
                    let veneer = self.here();
                    self.patches.push(Patch { at: veneer, field: Field::Far32, to: Label::Data(row) });
                    self.far_adr(rd);
                    let back = Field::Imm26.patch(enc::b(0), i64::from(patch.at) + 4 - i64::from(self.here())).ok_or(too_far)?;
                    self.put(back);
                    self.set(patch.at, enc::b(0));
                    self.patches[index as usize] = Patch { at: patch.at, field: Field::Imm26, to: Label::At(veneer) };
                }
                Label::Literal(literal) => {
                    let at = match copies.iter().find(|&&(to, _)| to == patch.to) {
                        Some(&(_, at)) => at,
                        None => {
                            let at = self.word_copy();
                            let place = self.fixups[literal as usize].place;
                            self.far.fixups.push(Fixup { at, place });
                            copies.push((patch.to, at));
                            at
                        }
                    };
                    self.patches[index as usize].to = Label::At(at);
                }
                Label::Word(word) => {
                    let at = match copies.iter().find(|&&(to, _)| to == patch.to) {
                        Some(&(_, at)) => at,
                        None => {
                            let at = self.word_copy();
                            let to = self.words[word as usize];
                            self.relocs.push(Reloc { at, to, addend: 0 });
                            copies.push((patch.to, at));
                            at
                        }
                    };
                    self.patches[index as usize].to = Label::At(at);
                }
                Label::At(_) => {}
            }
        }
        if fallthrough {
            let word = Field::Imm26.patch(enc::b(0), i64::from(self.here()) - i64::from(over)).ok_or(too_far)?;
            self.set(over, word);
        }
        Ok(())
    }

    /// Eight zero bytes at the next 8-aligned offset (a `nop` before them when needed), for
    /// placement to fill: their offset.
    fn word_copy(&mut self) -> u32 {
        if self.here() % 8 != 0 {
            self.put(NOP);
        }
        let at = self.here();
        self.code.extend_from_slice(&[0; 8]);
        at
    }
}
