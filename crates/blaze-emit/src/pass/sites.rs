//! The record form of the pass (docs/v2/plan/design/run.md, "End state", "The code record"):
//! the same forward pass, but no address word is placed after the code. What the code names
//! outside itself is an import site the segment's layout completes, and data holding an
//! address goes to the record's relro part.
//!
//! | reference | literal-word form (lib.rs) | record form |
//! |---|---|---|
//! | a call of a function | `ldr x16, <literal>; [ldr x16, [x16]]; blr x16` | `adrp x16, 0; ldr x16, [x16]; blr x16`: a call site |
//! | a system routine | `ldr x16, <literal>; blr x16` | the same call site, its target the routine's C name |
//! | `func_addr` | `ldr x9, <literal>` | `adrp x9, 0; ldr x9, [x9]`: a load site of the function |
//! | `data_addr` of a static, a vtable | `ldr x9, <literal>` | a load site |
//! | the thread-local descriptor, getter and offset | `ldr xN, <literal>` | a load site each (the offset's import word holds the offset) |
//! | `data_addr` of own data holding no address | `adr x9, <row>` | the same: the row follows the instructions |
//! | `data_addr` of own data holding an address | `adr x9, <row>`, the row's words filled in place | `adrp x9, 0; add x9, x9, #0`: a relro site; the row is in the relro part |
//! | an address in a data row | a word of the code placement fills | a word of the relro part, naming its target or the record's own code or relro byte |
//! | a source position in a data row | a zero field and a fixup in code | a zero field and its item-relative position row in code or relro |
//!
//! Every `adrp` is emitted with a zero page and every `ldr`/`add` with a zero offset: the
//! reach (`adrp`'s 4 GiB, the page offset scaled by 8) is checked where the address is known,
//! by `blaze_code::image::lay_out`, never assumed here.
//!
//! The pass keeps what it found as rows of reference places ([`Form`]); `crate::record` turns
//! them into `blaze_code` targets by symbol.

use core::alloc::Allocator;

use blaze_code::record::{PositionField, PositionPart, PositionRow};
use blaze_lir::func::{DataId, RefId, RefKind};
use blaze_own::Vec;

use super::{DATA_ALIGN_PAST, Label, MOST_DATA_ALIGN, Pass, unsupported};
use crate::{
    EmitError, RelocTo,
    enc::{self, Reg, Size, X16},
};

/// What a site reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SiteTo {
    /// The import word of what `RelocTo` names ([`RelocTo::Code`] never: own data is reached
    /// PC-relatively or through the relro part).
    Import(RelocTo),
    /// The relro row placed `i`th (an index into [`Form::data_at`]).
    Relro(u32),
}

/// Instructions of the code that the segment's layout completes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SiteRow {
    /// The `adrp`'s offset from the code's first byte.
    pub(crate) at: u32,
    /// Whether `blr x16` follows the load of `x16`: a call site.
    pub(crate) call: bool,
    pub(crate) to: SiteTo,
}

/// What a word of the relro part holds the address of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WordTo {
    Import(RelocTo),
    /// Byte `addend` of the record's own code (a data row holding no address).
    OwnCode,
    /// Byte `addend` of the record's own relro part.
    OwnRelro,
}

/// An 8-byte word of the relro part the image fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct WordRow {
    /// The word's offset in the relro part (8-aligned).
    pub(crate) at: u32,
    pub(crate) to: WordTo,
    pub(crate) addend: i64,
}

/// What the pass keeps in the record form, beside the code: every vector is the caller's, made
/// before the pass in the allocator it named.
pub(crate) struct Form<A: Allocator> {
    /// In the order emitted, so by offset.
    pub(crate) sites: Vec<SiteRow, A>,
    /// The data rows holding addresses, each at its alignment (8 at least, a 4 KiB page at
    /// most) from the part's first byte.
    pub(crate) relro: Vec<u8, A>,
    /// log2 of the alignment the code's first byte is placed at: the most of the code's data
    /// rows' (blaze-code record.rs `CodeRecord::code_align`), at least 16 bytes.
    pub(crate) code_align: u8,
    /// log2 of the alignment the relro part's first byte is placed at, as `code_align` for
    /// its rows, at least 8 bytes.
    pub(crate) relro_align: u8,
    pub(crate) words: Vec<WordRow, A>,
    pub(crate) positions: Vec<PositionRow, A>,
    /// Per placed data row, where it starts: in the code for a row holding no address, in the
    /// relro part for one that does.
    pub(crate) data_at: Vec<u32, A>,
    /// The words naming another data row, resolved once every row is placed: (the word, the
    /// row's index among the placed, the addend).
    inner: Vec<(u32, u32, i64), A>,
    /// The bytes of instructions: what follows in the code is the data rows holding no
    /// address, and nothing else.
    pub(crate) insts: u32,
    /// The calls' unwind rows (pass/unwind.rs).
    pub(crate) unwind: super::unwind::Rows<A>,
    /// Per `unreachable` instruction in layout order, its `brk`'s offset and where the value
    /// a switch fell through on is held there (pass.rs `held_at_unreachable`): what a build's
    /// `TrapSource` ordinal names (record.rs makes the trap rows).
    pub(crate) unreachables: Vec<(u32, blaze_code::record::Held), A>,
}

impl<A: Allocator + Clone> Form<A> {
    pub(crate) fn new_in(alloc: A) -> Form<A> {
        Form {
            sites: Vec::new_in(alloc.clone()),
            relro: Vec::new_in(alloc.clone()),
            code_align: blaze_code::record::LEAST_CODE_ALIGN,
            relro_align: blaze_code::record::LEAST_RELRO_ALIGN,
            words: Vec::new_in(alloc.clone()),
            positions: Vec::new_in(alloc.clone()),
            data_at: Vec::new_in(alloc.clone()),
            inner: Vec::new_in(alloc.clone()),
            insts: 0,
            unwind: super::unwind::Rows::new_in(alloc.clone()),
            unreachables: Vec::new_in(alloc),
        }
    }
}

/// Why the record form refuses a data row whose address is not on a word of the relro part.
pub(crate) const UNALIGNED_ADDRESS: &str =
    "an address at an offset that is not 8-aligned in its row (the image binds whole aligned words of the relro part)";

impl<A: Allocator> Pass<'_, '_, A> {
    /// Whether the pass emits the record form.
    pub(super) fn in_record(&self) -> bool {
        self.record.is_some()
    }

    fn site(&mut self, at: u32, call: bool, to: SiteTo) {
        if let Some(form) = self.record.as_mut() {
            form.sites.push(SiteRow { at, call, to });
        }
    }

    /// `adrp rt, 0; ldr rt, [rt]`: a load site, `rt` the value of `to`'s import word (an
    /// address; for [`RelocTo::TlsIndex`] the index).
    pub(super) fn import_load(&mut self, rt: Reg, to: RelocTo) {
        let at = self.here();
        self.put(enc::adrp(rt, 0));
        self.put(enc::ldr(Size::X, rt, rt, 0));
        self.site(at, false, SiteTo::Import(to));
    }

    /// `adrp x16, 0; ldr x16, [x16]`, the first two instructions of a call site: the caller
    /// puts `blr x16` next, with nothing between.
    pub(super) fn import_callee(&mut self, to: RelocTo) {
        let at = self.here();
        self.put(enc::adrp(X16, 0));
        self.put(enc::ldr(Size::X, X16, X16, 0));
        self.site(at, true, SiteTo::Import(to));
    }

    /// `adrp rd, 0; add rd, rd, #0`: a relro site, `rd` the address of the data row placed
    /// `index`th, which holds an address and so lives in the relro part.
    pub(super) fn relro_addr(&mut self, rd: Reg, index: u32) {
        let at = self.here();
        self.put(enc::adrp(rd, 0));
        self.put(enc::add_imm(rd, rd, 0, false));
        self.site(at, false, SiteTo::Relro(index));
    }

    /// The place of function reference `r`, refused unless it is a function's.
    pub(super) fn func_place(&self, inst: usize, op: blaze_lir::Op, r: RefId) -> Result<u32, EmitError> {
        let rec = self.view.reference(r);
        match rec.kind == RefKind::Func as u32 {
            true => Ok(rec.place),
            false => Err(unsupported(inst, op, "a reference to a static or thread-local where a function is called or its address taken")),
        }
    }

    /// [`Pass::finish`] in the record form: the data rows holding no address follow the
    /// instructions, the rows holding one go to the relro part with a word row per address,
    /// then every field is patched as in the literal-word form (the frame's size and its
    /// stack-probe tail were placed by `place_frame` before this, inside the instructions).
    pub(super) fn finish_record(&mut self) -> Result<(), EmitError> {
        let Some(form) = self.record.take() else {
            return Err(EmitError::Record { why: "the record form's rows are gone before the pass finished" });
        };
        // Nothing may reach a literal word: there is none in this form.
        for patch in self.patches.iter() {
            if matches!(patch.to, Label::Literal(_) | Label::Word(_)) {
                return Err(EmitError::Record {
                    why: "a load of a literal word after the code (a reference the record form has no import site for)",
                });
            }
        }
        if !self.fixups.is_empty() || !self.relocs.is_empty() || !self.words.is_empty() {
            return Err(EmitError::Record { why: "an address word asked of placement (the record form places none)" });
        }
        form.insts = self.here();
        let view = self.view;
        let too_large = EmitError::TooLarge { what: "the relro part (4 GiB)" };
        let mut at = 0;
        // The list grows as it is walked: a row reached only from another row's address.
        while at < self.placed.len() {
            let row = self.placed[at];
            let rec = view.datas[row as usize];
            if rec.align > MOST_DATA_ALIGN {
                return Err(EmitError::Data { row, why: DATA_ALIGN_PAST });
            }
            let align = 8usize.max(1 << rec.align);
            // The row's part asks its placement for the row's alignment (record.rs
            // `code_align`, `relro_align`): its offset there is a multiple of it.
            let log2 = rec.align as u8;
            let relocs = view.data_relocs_of(DataId(row));
            let bytes = view.data_bytes_of(DataId(row));
            let (part, start, placed) = if relocs.is_empty() {
                form.code_align = form.code_align.max(log2);
                self.pad_to(align);
                let start = self.code.len();
                self.code.extend_from_slice(bytes);
                (PositionPart::Code, start, &mut self.code[start..])
            } else {
                form.relro_align = form.relro_align.max(log2);
                while form.relro.len() % align != 0 {
                    form.relro.push(0);
                }
                let start = form.relro.len();
                form.relro.extend_from_slice(bytes);
                (PositionPart::Relro, start, &mut form.relro[start..])
            };
            let start = u32::try_from(start)
                .map_err(|_| EmitError::TooLarge { what: "a data row's offset in its record part (4 GiB)" })?;
            form.data_at.push(start);
            for p in view.data_positions_of(DataId(row)) {
                let field = match blaze_lir::PositionField::from_word(p.field) {
                    Some(blaze_lir::PositionField::Line) => PositionField::Line,
                    Some(blaze_lir::PositionField::Col) => PositionField::Col,
                    None => return Err(EmitError::Data { row, why: "a source position of no known field" }),
                };
                let offset = p.offset as usize;
                let slot = placed.get_mut(offset..).and_then(|tail| tail.get_mut(..4))
                    .ok_or(EmitError::Data { row, why: "a source position past its row's end" })?;
                slot.fill(0);
                let at = start.checked_add(p.offset)
                    .ok_or(EmitError::TooLarge { what: "a source position's offset in its record part (4 GiB)" })?;
                form.positions.push(PositionRow { part, at, item: p.item_key(), site: p.site, field });
            }
            for r in relocs {
                if u64::from(r.offset) + 8 > u64::from(rec.bytes_len) {
                    return Err(EmitError::Data { row, why: "a relocation past its row's end" });
                }
                if r.offset % 8 != 0 {
                    return Err(EmitError::Data { row, why: UNALIGNED_ADDRESS });
                }
                let word = start.checked_add(r.offset).ok_or(too_large)?;
                // The image writes the whole word: the record's bytes there are zero whatever
                // the row held.
                form.relro[word as usize..word as usize + 8].fill(0);
                let target = view.reference(RefId(r.target));
                let to = match RefKind::from_word(target.kind) {
                    Some(RefKind::Static) => RelocTo::Static(target.place),
                    Some(RefKind::Vtable) => RelocTo::Vtable(target.place),
                    Some(RefKind::Func) => RelocTo::Func(target.place),
                    Some(RefKind::Data) => {
                        let index = self.place_data(target.place);
                        form.inner.push((word, index, r.addend));
                        continue;
                    }
                    Some(RefKind::Tls) | None => {
                        return Err(EmitError::Data { row, why: "the address of a thread-local in data (it differs per thread)" });
                    }
                };
                form.words.push(WordRow { at: word, to: WordTo::Import(to), addend: r.addend });
            }
            at += 1;
        }
        for i in 0..form.inner.len() {
            let (word, index, addend) = form.inner[i];
            let row = self.placed[index as usize];
            let to = match view.data_relocs_of(DataId(row)).is_empty() {
                true => WordTo::OwnCode,
                false => WordTo::OwnRelro,
            };
            let addend = i64::from(form.data_at[index as usize])
                .checked_add(addend)
                .ok_or(EmitError::Data { row, why: "an addend past what a word holds" })?;
            form.words.push(WordRow { at: word, to, addend });
        }
        self.settle(0, 0, &form.data_at)
    }
}
