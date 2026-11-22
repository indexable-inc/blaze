//! The record form's promise, checked from a record's bytes alone (docs/v2/plan/design/run.md,
//! "End state", "The code record"): no instruction of a code record loads a literal word, and
//! every `adrp` begins a site row of the kind it is written as. [`emit_record_measured`] gives
//! the count of instruction bytes at the code's start that [`no_address_word`] needs (what
//! follows them is the function's data holding no address).
//!
//! The emitter's own tests check more (record_tests.rs `holds`: the data after the
//! instructions, the round trip); this is the part a caller outside the crate checks a record
//! by, for every function a run path builds (crates/blaze-ladder/tests/code_records.rs).

use core::fmt;

use blaze_code::record::{CodeRecord, CodeView, SiteKind, TrapSource};
use blaze_lir::Verified;
use blaze_own::Alloc;

use crate::{BULK_INLINE_MOST, Called, Isa, Names, RecordEmitError, RoutinePlaces, record::emit_record_sourced};

/// [`crate::emit_record`], and the bytes of instructions at the start of the record's code;
/// the routines the record calls ([`Called`]) last. Each of the build's `traps` becomes its
/// `unreachable`'s trap row (blaze-code record.rs `TrapSource`).
pub fn emit_record_measured<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    isa: Isa,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    emit_record_sourced(func, names, routines, isa, BULK_INLINE_MOST, traps, alloc)
}

/// The first broken promise of a record's instructions, by its offset in the code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AddressWord {
    /// `insts` is not a whole number of instructions inside the code's `code` bytes.
    Instructions { insts: u32, code: u32 },
    /// A load of a literal word (A64 "load register (literal)") at `at`: an address the
    /// placement would have to fill.
    LiteralLoad { at: u32 },
    /// An `adr` at `at` reaches `to`, outside the data after the instructions.
    AdrOutside { at: u32, to: i64 },
    /// An `adrp` at `at` that no site row names, or a site row past the last `adrp`.
    UnnamedPage { at: u32 },
    /// The site at `at` is not written as its kind says (`what`: the instruction expected).
    SiteForm { at: u32, what: &'static str },
}

impl fmt::Display for AddressWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "a code record holds no address (run.md, \"The code record\")";
        match *self {
            AddressWord::Instructions { insts, code } => write!(f, "{insts} bytes of instructions in {code} bytes of code: {rule}"),
            AddressWord::LiteralLoad { at } => write!(f, "a load of a literal word at code byte {at}: {rule}"),
            AddressWord::AdrOutside { at, to } => write!(f, "the `adr` at code byte {at} reaches {to}, not the function's own data: {rule}"),
            AddressWord::UnnamedPage { at } => write!(f, "an `adrp` at code byte {at} with no site row (or a site row with no `adrp`): {rule}"),
            AddressWord::SiteForm { at, what } => write!(f, "the site at code byte {at} is not `{what}`: {rule}"),
        }
    }
}

impl core::error::Error for AddressWord {}

fn inst(code: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

/// Where an `adr` at `at` reaches: its own offset, or for the far own-data form (pass/reach.rs:
/// `adr xD, #0; movz x10, #lo; movk x10, #hi, lsl #16; add xD, xD, x10`, the whole form inside
/// the first `end` bytes) the `adr`'s offset plus `hi << 16 | lo`. `word` is the `adr`.
pub(crate) fn adr_reaches(code: &[u8], end: usize, at: usize, word: u32) -> i64 {
    // `adr`: 21 signed bits, the low 2 in 29..31.
    let imm = (((word >> 5) & 0x7_ffff) << 2) | ((word >> 29) & 3);
    let near = at as i64 + i64::from(((imm << 11) as i32) >> 11);
    let rd = word & 31;
    if imm != 0 || at + 16 > end {
        return near;
    }
    let (movz, movk, add) = (inst(code, at + 4), inst(code, at + 8), inst(code, at + 12));
    let (lo, hi) = ((movz >> 5) & 0xffff, (movk >> 5) & 0xffff);
    let far = movz == crate::enc::movz(crate::enc::X10, lo, 0)
        && movk == crate::enc::movk(crate::enc::X10, hi, 1)
        && add == crate::enc::add(rd, rd, crate::enc::X10);
    match far {
        true => at as i64 + i64::from((hi << 16) | lo),
        false => near,
    }
}

/// Checks the first `insts` bytes of `view`'s code (the instructions): no load of a literal
/// word (bits 29..27 `011`, bits 25..24 `00`); every `adr` reaches the data after the
/// instructions (the far form's `adr` with its offset added: [`adr_reaches`]); every `adrp` is the first instruction of the next site row, written with a
/// zero page and followed by `ldr xN, [xN]` (a load or call site; a call's register is `x16`
/// and `blr x16` ends it) or `add xN, xN, #0` (a relro site); and no site row is left over.
/// The number of sites on success.
pub fn no_address_word(view: &CodeView<'_>, insts: u32) -> Result<usize, AddressWord> {
    let code = view.code();
    let end = insts as usize;
    if end % 4 != 0 || end > code.len() {
        return Err(AddressWord::Instructions { insts, code: code.len() as u32 });
    }
    let (mut at, mut site) = (0usize, 0usize);
    while at < end {
        let word = inst(code, at);
        let here = at as u32;
        if word & 0x3b00_0000 == 0x1800_0000 {
            return Err(AddressWord::LiteralLoad { at: here });
        }
        if word & 0x9f00_0000 == 0x1000_0000 {
            let to = adr_reaches(code, end, at, word);
            if to < end as i64 || to > code.len() as i64 {
                return Err(AddressWord::AdrOutside { at: here, to });
            }
        }
        if word & 0x9f00_0000 != 0x9000_0000 {
            at += 4;
            continue;
        }
        if site >= view.sites() || view.site(site).at != here {
            return Err(AddressWord::UnnamedPage { at: here });
        }
        let row = view.site(site);
        let rd = word & 31;
        if word != 0x9000_0000 | rd {
            return Err(AddressWord::SiteForm { at: here, what: "adrp xN, 0" });
        }
        if at + row.kind.len() as usize > end {
            return Err(AddressWord::SiteForm { at: here, what: "a whole site inside the instructions" });
        }
        let second = inst(code, at + 4);
        match row.kind {
            SiteKind::Relro if second != 0x9100_0000 | (rd << 5) | rd => {
                return Err(AddressWord::SiteForm { at: here, what: "add xN, xN, #0" });
            }
            SiteKind::Load | SiteKind::Call if second != 0xf940_0000 | (rd << 5) | rd => {
                return Err(AddressWord::SiteForm { at: here, what: "ldr xN, [xN]" });
            }
            _ => {}
        }
        if row.kind == SiteKind::Call && (rd != 16 || inst(code, at + 8) != 0xd63f_0200) {
            return Err(AddressWord::SiteForm { at: here, what: "a load into x16, then blr x16" });
        }
        site += 1;
        at += row.kind.len() as usize;
    }
    if site != view.sites() {
        let at = if site < view.sites() { view.site(site).at } else { insts };
        return Err(AddressWord::UnnamedPage { at });
    }
    Ok(site)
}
