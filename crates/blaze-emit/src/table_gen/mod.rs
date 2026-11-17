//! The template table's first generator (backend-next.md D99; the backend wave's piece T1):
//! blaze-emit's own encoders (enc.rs), each run with PLACEHOLDER operands, make every template of
//! the Fast tier's table, in piece 0's form (crates/blaze-tier/src/table.rs). Nothing here holds a
//! bit position by hand: [`discover`] flips one bit of one operand at a time, runs the encoder
//! again and diffs the bytes; the bits that changed are that operand bit's field. A bit no flip
//! reaches, a bit two operand bits reach, or an encoder that is not linear in its operands is a
//! [`GenError`] naming the pattern. The composer (blaze-emit's `compose`, piece K2) reads the
//! table only through [`blaze_tier::Table`]; [`EncodedTable`] is that trait over the bytes.
//!
//! # What a template's operands are (the conventions the composer looks patterns up by)
//!
//! - `width` is the operation's width in bits: an integer operation's result's; a compare's,
//!   a branch's fused compare's and `ptr_add`'s, its (offset) operands'; 0 where none (`jump`,
//!   `call`, `trap`). A conversion (`zext`, `sext`, `trunc`) is at its RESULT's width with the
//!   SOURCE width as its `variant` (the conversion's kind, as table.rs says a variant is).
//! - `icmp`'s variant is its condition's byte (`IntCond::ALL`'s order, as LIR stores it).
//! - Every integer value is held as LIR's baseline holds it (lib.rs, "Values"): zero-extended to
//!   64 bits; a template narrows its result and sign-extends where it reads the sign.
//! - `load` and `store` take their offset as an immediate operand (LIR's `Offset` field):
//!   `load [ptr, Imm]`, `store [value, ptr, Imm]`, in the classes `S9` (`ldur`/`stur`) and
//!   `U12Scaled{0..3}` (`ldr`/`str` of `1 << scale` bytes, the offset in units).
//! - `br` serves its THEN edge only: `[cond, Block]` (`cbnz`); the composer makes the else edge
//!   with `jump`'s template or by falling through. Fused with its compare (D100 point 6),
//!   `[None, Block, a, b]` with `inner = icmp` at operand 0, whose operands follow the outer two.
//! - `load`/`store` of a stack slot fused with its `slot_addr`: `[None, None, Slot, None]` and
//!   `[value, None, None, Slot]`; the slot's field is a `Frame` field of the slot's offset PLUS
//!   the access's offset, in units of the access's size (the composer adds them).
//! - A constant: `iconst [Imm(Mov16)]` (one `movz`); any 64-bit constant `iconst`/`iconst_wide`
//!   `[Imm(I64)]` (`movz` and three `movk`: the class table.rs names for x86-64's `movabs` is
//!   the one class holding any 64-bit value, so arm64's four-word form uses it too).
//! - A shift by an immediate holds the amount below the width; LIR takes an amount modulo the
//!   width (pass.rs, `Op::Shl`), so the composer reduces a constant amount first.
//! - A call is `call [Site(Call)]` (`adrp x16; ldr x16; blr x16`) and `call_indirect [Reg]`;
//!   both clobber every register a call does not keep. Argument and result moves are the
//!   composer's.
//! - Operand numbers in fields: inputs 0..4, the result [`RESULT`], temporaries [`TEMP0`] and
//!   [`TEMP1`] (the composer gives them from the description's scratch set).
//!
//! - The composer's own instructions (frame steps, moves, float cells, `ret`, the poll), `xor` by
//!   a logical immediate, shifted-register operands (`[a, None, b, Imm(amount)]` with the shift as
//!   `inner` at operand 1) and `cbz` (`br` variant `BR_IF_ZERO`: the else edge) are
//!   arm64_glue.rs's.
//!
//! Not generated, by name: a register-offset load (`ldr_lsl3`, a two-level tree a one-level
//! [`blaze_tier::Inner`] cannot say), a shift left by an immediate
//! (`lsl` is `ubfm` of `64 - s`: not linear in `s`, so no field holds it), the atomics, the
//! 128-bit and overflow operations, and float compares and conversions. A pattern the table does
//! not hold is `TableError::Missing` at the composer, which names it.
//!
//! # The record form's choices piece 0 left open
//!
//! The head's `fields_len` counts FIELDS ([`FIELD_BYTES`] each); a row's `bytes_at` is an offset
//! in the template bytes and its `fields_at` an index in the fields. Rows, their bytes and their
//! fields are written in the rows' sorted order. A field's kind byte: `Reg` 0, `Imm` 1, `Site` 2
//! (argument the `SiteKind` byte), `Branch` 3 (argument `scale`, and the head's last byte, which
//! table.rs writes as 0, holds `from`), `Frame` 4 (argument `scale`). A `Site` field covers the
//! whole site (`at` its first bit, `width` its `SiteKind::len` in bits). A register class byte
//! is `Gpr` 0, `Fpr` 1; arch `Arm64` 0, `X86_64` 1; os `MacOs` 0, `Linux` 1.

use core::fmt;

use blaze_code::record::SiteKind;
use blaze_lir::Op;
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::table::{FIELD_BYTES, RESULT, ROW_BYTES, TABLE_MAGIC, TEMP0, TEMP1};
use blaze_tier::{
    Arch, Field, FieldKind, Form, ImmClass, Inner, Machine, Os, Pattern, RegClass, RegSet, Source, Table, TableError,
    TableHead, Template,
};

pub(crate) mod arm64;
pub mod x86_64;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod x86_64_tests;

/// The most bytes one template holds.
pub const MOST_BYTES: usize = 64;
/// The operand numbers a field may name: 0..4, [`RESULT`], [`TEMP0`], [`TEMP1`].
pub const OPERANDS: usize = 7;
/// The bytes of the head: magic, four bytes of machine and source, features, three counts.
pub const HEAD_BYTES: usize = 8 + 4 + 8 + 4 + 4 + 4;

/// One value per operand number, as an encoder takes them: a register's hardware number, an
/// immediate in its class's encoding, a branch's displacement in its units.
pub type Vals = [u64; OPERANDS];

/// The placeholder register of each operand: distinct, never 31 (`sp`/`xzr`), never the scratch
/// pair x16/x17 the templates name themselves. Each has an even number of set bits, so one flip
/// of one bit of one never makes another (two inputs in one register would be one value to a
/// compiler generating through [`discover`], which folds `x - x`: the LLVM generator, T2); and
/// no one-bit flip of any reaches x18 (reserved on macOS), x29, x30 or x31.
const PLACEHOLDER_REG: [u64; OPERANDS] = [3, 5, 6, 9, 10, 12, 20];

/// The placeholder of an immediate operand, masked to its slot's bits: never 0 (a compiler folds
/// `a + 0` to a move), at least two bits set in every low run, so no single flip makes it 0.
const PLACEHOLDER_IMM: u64 = 0x5555_5555_5555_5555;

/// Operand `operand`'s value in the second linearity probe: [`IRREGULAR`] rotated by a byte per
/// operand number, so no two operands share a register (5 bits: 15, 13, 14, 12, 7, 5, 25).
pub const fn irregular_of(operand: u8) -> u64 {
    IRREGULAR.rotate_right(8 * operand as u32)
}

/// A template's bytes as an encoder writes them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Code {
    bytes: [u8; MOST_BYTES],
    len: usize,
    /// An encoder wrote past [`MOST_BYTES`]: [`discover`] refuses the pattern by name.
    over: bool,
}

impl Code {
    pub const fn new() -> Code {
        Code { bytes: [0; MOST_BYTES], len: 0, over: false }
    }

    /// One arm64 instruction word, little-endian.
    pub fn word(&mut self, word: u32) {
        let le = word.to_le_bytes();
        match self.bytes.get_mut(self.len..self.len + le.len()) {
            Some(to) => {
                to.copy_from_slice(&le);
                self.len += le.len();
            }
            None => self.over = true,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Default for Code {
    fn default() -> Code {
        Code::new()
    }
}

/// What a placeholder operand's field is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotKind {
    Reg,
    Imm,
    /// A branch displacement in units of `1 << scale` bytes.
    Branch { scale: u8 },
    /// A frame offset from `sp` in units of `1 << scale` bytes.
    Frame { scale: u8 },
}

/// One operand an encoder takes, and how many low bits of its value it encodes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    pub operand: u8,
    pub kind: SlotKind,
    pub bits: u8,
}

/// An import site in a template: its first byte and kind. Its own fields are the segment
/// layout's (blaze-code), so nothing is discovered in it but the registers around it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SiteAt {
    pub operand: u8,
    pub kind: SiteKind,
    pub byte: u8,
}

/// One template to generate: the pattern it serves, its operands, and the encoder calls that make
/// its bytes from operand values.
pub struct Recipe<'r> {
    pub pattern: Pattern,
    pub slots: &'r [Slot],
    pub site: Option<SiteAt>,
    /// Fixed registers it writes besides its result (a call's, the site's x16).
    pub clobbers: RegSet,
    /// The bytes of one instruction (arm64: 4): a branch field's `from` is the start of the
    /// instruction holding it. 0 where instructions differ in length.
    pub insn_bytes: u8,
    pub encode: &'r dyn Fn(&Vals, &mut Code),
}

/// Why no table was generated. Every variant names its pattern where it has one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenError {
    /// No generator for this machine here (x86-64 is piece X's).
    Machine { machine: Machine },
    /// Flipping bit `bit` of `operand` changed no byte: the encoder ignores it.
    Undiscovered { pattern: Pattern, operand: u8, bit: u8 },
    /// The encoder is not linear in `operand` (a flip changed the length, or the bytes are not
    /// the cleared template with the fields filled); `operand` is `u8::MAX` for the probe of all
    /// operands at once.
    Nonlinear { pattern: Pattern, operand: u8 },
    /// Template bit `at` is reached by two operand bits.
    Overlap { pattern: Pattern, at: u16 },
    /// The template is past [`MOST_BYTES`], or its site past its bytes.
    TooLong { pattern: Pattern },
    /// A slot names no operand number, or holds 0 or more than 64 bits.
    Slot { pattern: Pattern, operand: u8 },
    /// A branch field in a template whose instructions differ in length.
    BranchFrom { pattern: Pattern },
    /// Two templates serve one pattern.
    Repeated { pattern: Pattern },
    /// A count past its field in the record form.
    TooMany { what: &'static str },
}

/// A pattern as an error shows it.
struct Shown(Pattern);

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = self.0;
        write!(f, "`{}` at width {} variant {} ({:?} -> {:?}", p.op.name(), p.width, p.variant, p.operands, p.result)?;
        match p.inner {
            Some(inner) => write!(f, ", fused `{}` at operand {})", inner.op.name(), inner.operand),
            None => f.write_str(")"),
        }
    }
}

impl fmt::Display for GenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            GenError::Machine { machine } => {
                write!(f, "no template generator for {:?} on {:?}", machine.arch, machine.os)
            }
            GenError::Undiscovered { pattern, operand, bit } => {
                write!(f, "template {}: bit {bit} of operand {operand} reaches no bit of the code", Shown(pattern))
            }
            GenError::Nonlinear { pattern, operand: u8::MAX } => {
                write!(f, "template {}: the encoder is not linear in its operands together", Shown(pattern))
            }
            GenError::Nonlinear { pattern, operand } => {
                write!(f, "template {}: the encoder is not linear in operand {operand}", Shown(pattern))
            }
            GenError::Overlap { pattern, at } => {
                write!(f, "template {}: code bit {at} is reached by two operand bits", Shown(pattern))
            }
            GenError::TooLong { pattern } => {
                write!(f, "template {}: past {MOST_BYTES} bytes, or a site past its bytes", Shown(pattern))
            }
            GenError::Slot { pattern, operand } => {
                write!(f, "template {}: slot of operand {operand} names no operand or holds 0 or over 64 bits", Shown(pattern))
            }
            GenError::BranchFrom { pattern } => {
                write!(f, "template {}: a branch field where instructions differ in length", Shown(pattern))
            }
            GenError::Repeated { pattern } => write!(f, "two templates serve {}", Shown(pattern)),
            GenError::TooMany { what } => write!(f, "the table's {what} is past its field"),
        }
    }
}

impl core::error::Error for GenError {}

/// A planted bug the tests catch (rule 8): the generator dropping each field's `from_bit`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    DropFromBit,
}

/// The placeholder values of `slots`: a distinct register per register operand,
/// [`PLACEHOLDER_IMM`] masked to its bits per immediate operand, 0 for the rest.
pub fn placeholders(slots: &[Slot]) -> Vals {
    let mut vals = [0; OPERANDS];
    for slot in slots {
        match (slot.kind, vals.get_mut(usize::from(slot.operand))) {
            (SlotKind::Reg, Some(at)) => *at = PLACEHOLDER_REG[usize::from(slot.operand)],
            (SlotKind::Imm, Some(at)) => *at = PLACEHOLDER_IMM & mask(u32::from(slot.bits)),
            _ => {}
        }
    }
    vals
}

const fn mask(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

fn bit_of(code: &[u8], at: usize) -> bool {
    code[at / 8] >> (at % 8) & 1 == 1
}

fn set_bit(code: &mut [u8], at: usize, on: bool) {
    let bit = 1u8 << (at % 8);
    match on {
        true => code[at / 8] |= bit,
        false => code[at / 8] &= !bit,
    }
}

/// `value`'s bits `field.from_bit..field.from_bit + field.width` written into the field's bits
/// of `code`. A `Site` field is the segment layout's and is left as it is.
pub fn fill_field(code: &mut [u8], field: &Field, value: u64) {
    if let FieldKind::Site(_) = field.kind {
        return;
    }
    for j in 0..usize::from(field.width) {
        let from = usize::from(field.from_bit) + j;
        let on = from < 64 && value >> from & 1 == 1;
        set_bit(code, usize::from(field.at) + j, on);
    }
}

/// Every field of `fields` filled from `vals` by its operand.
pub fn fill(code: &mut [u8], fields: &[Field], vals: &Vals) {
    for field in fields {
        if let Some(&value) = vals.get(usize::from(field.operand)) {
            fill_field(code, field, value);
        }
    }
}

/// The fields of one template found by flipping its operands' bits, appended to `fields` (sorted
/// by bit), and its bytes with every field's bits cleared, appended to `bytes`. Shared by every
/// generator (T2 runs LLVM's code through it). Refuses by name a bit no flip reaches, a bit two
/// reach, and an encoder that is not linear in its operands (two probes: the placeholders, and
/// a fixed irregular value in every operand).
pub fn discover<'a>(recipe: &Recipe<'_>, bytes: &mut AVec<'a, u8>, fields: &mut AVec<'a, Field>) -> Result<(), GenError> {
    let pattern = recipe.pattern;
    let base_vals = placeholders(recipe.slots);
    let mut base = Code::new();
    (recipe.encode)(&base_vals, &mut base);
    if base.over {
        return Err(GenError::TooLong { pattern });
    }
    let len = base.len;
    let bits = len * 8;
    // claims[at]: 0 where code bit `at` holds no operand bit, else `(slot + 1) << 8 | bit`.
    let mut claims = [0u16; MOST_BYTES * 8];
    for (s, slot) in recipe.slots.iter().enumerate() {
        if usize::from(slot.operand) >= OPERANDS || slot.bits == 0 || slot.bits > 64 {
            return Err(GenError::Slot { pattern, operand: slot.operand });
        }
        let tag = u16::try_from(s + 1).map_err(|_| GenError::TooMany { what: "slots of one template" })? << 8;
        for bit in 0..slot.bits {
            let mut vals = base_vals;
            vals[usize::from(slot.operand)] ^= 1u64 << bit;
            let mut flipped = Code::new();
            (recipe.encode)(&vals, &mut flipped);
            if flipped.over || flipped.len != len {
                return Err(GenError::Nonlinear { pattern, operand: slot.operand });
            }
            let mut seen = false;
            for (at, claim) in claims.iter_mut().enumerate().take(bits) {
                if bit_of(&base.bytes, at) == bit_of(&flipped.bytes, at) {
                    continue;
                }
                seen = true;
                if *claim != 0 {
                    return Err(GenError::Overlap { pattern, at: u16::try_from(at).unwrap_or(u16::MAX) });
                }
                *claim = tag | u16::from(bit);
            }
            if !seen {
                return Err(GenError::Undiscovered { pattern, operand: slot.operand, bit });
            }
        }
    }

    let first = fields.len();
    let insn_bits = usize::from(recipe.insn_bytes) * 8;
    let mut at = 0;
    while at < bits {
        let claim = claims[at];
        if claim == 0 {
            at += 1;
            continue;
        }
        let slot = recipe.slots[usize::from(claim >> 8) - 1];
        let mut width = 1;
        // A field runs while the next code bit holds the next bit of the same operand, within
        // one instruction.
        while at + width < bits
            && width < 64
            && (insn_bits == 0 || (at + width) % insn_bits != 0)
            && usize::from(claims[at + width]) == usize::from(claim) + width
        {
            width += 1;
        }
        let kind = match slot.kind {
            SlotKind::Reg => FieldKind::Reg,
            SlotKind::Imm => FieldKind::Imm,
            SlotKind::Frame { scale } => FieldKind::Frame { scale },
            SlotKind::Branch { scale } => {
                if recipe.insn_bytes == 0 {
                    return Err(GenError::BranchFrom { pattern });
                }
                let insn = usize::from(recipe.insn_bytes);
                let from = at / 8 / insn * insn;
                FieldKind::Branch { scale, from: u8::try_from(from).map_err(|_| GenError::TooLong { pattern })? }
            }
        };
        fields.push(Field {
            at: u16::try_from(at).map_err(|_| GenError::TooLong { pattern })?,
            width: u8::try_from(width).map_err(|_| GenError::TooLong { pattern })?,
            operand: slot.operand,
            from_bit: u8::try_from(claim & 0xff).map_err(|_| GenError::TooLong { pattern })?,
            kind,
        });
        at += width;
    }
    if let Some(site) = recipe.site {
        let site_bytes = usize::try_from(site.kind.len()).map_err(|_| GenError::TooLong { pattern })?;
        if usize::from(site.byte) + site_bytes > len || usize::from(site.operand) >= OPERANDS {
            return Err(GenError::TooLong { pattern });
        }
        fields.push(Field {
            at: u16::from(site.byte) * 8,
            width: u8::try_from(site_bytes * 8).map_err(|_| GenError::TooLong { pattern })?,
            operand: site.operand,
            from_bit: 0,
            kind: FieldKind::Site(site.kind),
        });
    }
    fields[first..].sort_unstable();

    let mut cleared = base.bytes;
    for (at, &claim) in claims.iter().enumerate().take(bits) {
        if claim != 0 {
            set_bit(&mut cleared, at, false);
        }
    }
    // Probe 1: the placeholders filled into the cleared bytes give the encoder's bytes back.
    let mut probe = cleared;
    fill(&mut probe[..len], &fields[first..], &base_vals);
    if probe[..len] != base.bytes[..len] {
        return Err(GenError::Nonlinear { pattern, operand: u8::MAX });
    }
    // Probe 2: an irregular value in every operand at once, each operand its own.
    let mut vals = base_vals;
    for slot in recipe.slots {
        vals[usize::from(slot.operand)] = irregular_of(slot.operand) & mask(u32::from(slot.bits));
    }
    let mut want = Code::new();
    (recipe.encode)(&vals, &mut want);
    let mut probe = cleared;
    fill(&mut probe[..len], &fields[first..], &vals);
    if want.over || want.len != len || probe[..len] != want.bytes[..len] {
        return Err(GenError::Nonlinear { pattern, operand: u8::MAX });
    }
    bytes.extend_from_slice(&cleared[..len]);
    Ok(())
}

/// No two 16-bit pieces equal, no byte repeated: a value that shows a field holding the wrong
/// bits of its operand.
pub(crate) const IRREGULAR: u64 = 0x0123_4567_89ab_cdef ^ 0x5a5a_0000_a5a5_0000;

/// One generated row before it is written: its template's bytes and fields are
/// `bytes[bytes_at..][..bytes_len]` and `fields[fields_at..][..fields_len]` of the buffers handed
/// to [`write_table`] with it.
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub pattern: Pattern,
    pub bytes_at: usize,
    pub bytes_len: usize,
    pub fields_at: usize,
    pub fields_len: usize,
    pub clobbers: RegSet,
}

/// Every recipe of `machine`'s table, handed to `each` in a fixed order: the patterns and slots
/// the LLVM generator (blaze-llvm table_gen.rs, T2) runs LLVM's code through. x86-64 places its
/// fields by the manual's rules, not by recipes: [`GenError::Machine`].
pub fn recipes(machine: Machine, each: &mut dyn FnMut(&Recipe<'_>) -> Result<(), GenError>) -> Result<(), GenError> {
    match machine.arch {
        Arch::Arm64 => arm64::recipes(each),
        Arch::X86_64 => Err(GenError::Machine { machine }),
    }
}

/// The table of `machine` in piece 0's record form, appended to `out`; its head (count and
/// BLAKE3 digest of the bytes appended). Deterministic: the same encoders give the same bytes.
pub fn generate<'a>(machine: Machine, out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<TableHead, GenError> {
    generate_planted(machine, out, alloc, Plant::None)
}

pub(crate) fn generate_planted<'a>(
    machine: Machine,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
    plant: Plant,
) -> Result<TableHead, GenError> {
    let mut rows: AVec<'a, Row> = Vec::new_in(alloc);
    let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
    let mut fields: AVec<'a, Field> = Vec::new_in(alloc);
    match machine.arch {
        Arch::Arm64 => arm64::recipes(&mut |recipe| {
            let (bytes_at, fields_at) = (bytes.len(), fields.len());
            discover(recipe, &mut bytes, &mut fields)?;
            if plant == Plant::DropFromBit {
                for field in &mut fields[fields_at..] {
                    field.from_bit = 0;
                }
            }
            rows.push(Row {
                pattern: recipe.pattern,
                bytes_at,
                bytes_len: bytes.len() - bytes_at,
                fields_at,
                fields_len: fields.len() - fields_at,
                clobbers: recipe.clobbers,
            });
            Ok(())
        })?,
        // x86-64's templates place their own fields by the manual's rules (x86_64.rs: x86's
        // rel32 counts from an instruction's END, which `discover`'s one-length origin cannot say).
        Arch::X86_64 => {
            let built = x86_64::templates(alloc).map_err(|error| match error {
                x86_64::X86Error::TooLong { pattern, .. } => GenError::TooLong { pattern },
                x86_64::X86Error::Repeated { pattern } => GenError::Repeated { pattern },
            })?;
            for template in built.iter() {
                let (bytes_at, fields_at) = (bytes.len(), fields.len());
                bytes.extend_from_slice(template.bytes());
                fields.extend_from_slice(template.fields());
                if plant == Plant::DropFromBit {
                    for field in &mut fields[fields_at..] {
                        field.from_bit = 0;
                    }
                }
                rows.push(Row {
                    pattern: template.pattern,
                    bytes_at,
                    bytes_len: template.bytes().len(),
                    fields_at,
                    fields_len: template.fields().len(),
                    clobbers: template.clobbers,
                });
            }
        }
    }
    write_table(machine, Source::Encoders, &mut rows, &bytes, &fields, out)
}

/// `rows` (sorted here by pattern; a pattern twice is [`GenError::Repeated`]) with their bytes and
/// fields, in piece 0's record form, appended to `out`; the head (count and BLAKE3 of the bytes
/// appended). The one writer of the form: the encoders' generator and LLVM's (T2) both end here.
pub fn write_table(
    machine: Machine,
    source: Source,
    rows: &mut [Row],
    bytes: &[u8],
    fields: &[Field],
    out: &mut AVec<'_, u8>,
) -> Result<TableHead, GenError> {
    rows.sort_unstable_by(|a, b| a.pattern.cmp(&b.pattern));
    for pair in rows.windows(2) {
        if pair[0].pattern == pair[1].pattern {
            return Err(GenError::Repeated { pattern: pair[0].pattern });
        }
    }

    let count = u32::try_from(rows.len()).map_err(|_| GenError::TooMany { what: "row count" })?;
    let bytes_len = u32::try_from(bytes.len()).map_err(|_| GenError::TooMany { what: "template bytes" })?;
    let fields_len = u32::try_from(fields.len()).map_err(|_| GenError::TooMany { what: "field count" })?;
    let start = out.len();
    out.extend_from_slice(&TABLE_MAGIC);
    out.extend_from_slice(&[arch_byte(machine.arch), os_byte(machine.os), source_byte(source), 0]);
    out.extend_from_slice(&machine.features.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&bytes_len.to_le_bytes());
    out.extend_from_slice(&fields_len.to_le_bytes());
    // The rows, with their bytes and fields laid out again in the rows' order.
    let (mut bytes_at, mut fields_at) = (0u32, 0u32);
    for row in rows.iter() {
        let row_bytes = u16::try_from(row.bytes_len).map_err(|_| GenError::TooMany { what: "a template's bytes" })?;
        let row_fields = u16::try_from(row.fields_len).map_err(|_| GenError::TooMany { what: "a template's fields" })?;
        write_pattern(out, &row.pattern);
        out.extend_from_slice(&bytes_at.to_le_bytes());
        out.extend_from_slice(&row_bytes.to_le_bytes());
        out.extend_from_slice(&fields_at.to_le_bytes());
        out.extend_from_slice(&row_fields.to_le_bytes());
        out.extend_from_slice(&row.clobbers.0.to_le_bytes());
        bytes_at += u32::from(row_bytes);
        fields_at += u32::from(row_fields);
    }
    for row in rows.iter() {
        out.extend_from_slice(&bytes[row.bytes_at..row.bytes_at + row.bytes_len]);
    }
    for row in rows.iter() {
        for field in &fields[row.fields_at..row.fields_at + row.fields_len] {
            write_field(out, field);
        }
    }
    let digest = blaze_blake3::hash(&out[start..]);
    Ok(TableHead { machine, source, count, digest })
}

const fn arch_byte(arch: Arch) -> u8 {
    match arch {
        Arch::Arm64 => 0,
        Arch::X86_64 => 1,
    }
}

const fn os_byte(os: Os) -> u8 {
    match os {
        Os::MacOs => 0,
        Os::Linux => 1,
    }
}

const fn source_byte(source: Source) -> u8 {
    match source {
        Source::Encoders => 0,
        Source::Llvm => 1,
    }
}

const fn class_byte(class: RegClass) -> u8 {
    match class {
        RegClass::Gpr => 0,
        RegClass::Fpr => 1,
    }
}

const fn site_byte(kind: SiteKind) -> u8 {
    match kind {
        SiteKind::Load => 0,
        SiteKind::Call => 1,
        SiteKind::Relro => 2,
    }
}

const SITE_KINDS: [SiteKind; 3] = [SiteKind::Load, SiteKind::Call, SiteKind::Relro];

/// Every immediate class by its byte (table.rs's discriminants).
const IMM_CLASSES: [ImmClass; 16] = [
    ImmClass::U12,
    ImmClass::U12Lsl12,
    ImmClass::Logical32,
    ImmClass::Logical64,
    ImmClass::Shift5,
    ImmClass::Shift6,
    ImmClass::Mov16,
    ImmClass::S9,
    ImmClass::U12Scaled0,
    ImmClass::U12Scaled1,
    ImmClass::U12Scaled2,
    ImmClass::U12Scaled3,
    ImmClass::U12Scaled4,
    ImmClass::S8,
    ImmClass::S32,
    ImmClass::I64,
];

/// `ImmClass` is `#[repr(u8)]`: its byte is its discriminant, and [`IMM_CLASSES`] is in that
/// order (pinned by `imm_classes_are_in_byte_order`).
const fn imm_byte(class: ImmClass) -> u8 {
    class as u8
}

fn form_bytes(form: Form) -> [u8; 2] {
    match form {
        Form::None => [0, 0],
        Form::Reg(class) => [1, class_byte(class)],
        Form::Imm(class) => [2, imm_byte(class)],
        Form::Site(kind) => [3, site_byte(kind)],
        Form::Block => [4, 0],
        Form::Slot => [5, 0],
    }
}

fn form_of(kind: u8, arg: u8) -> Option<Form> {
    match (kind, arg) {
        (0, 0) => Some(Form::None),
        (1, 0) => Some(Form::Reg(RegClass::Gpr)),
        (1, 1) => Some(Form::Reg(RegClass::Fpr)),
        (2, arg) => IMM_CLASSES.get(usize::from(arg)).map(|&c| Form::Imm(c)),
        (3, arg) => SITE_KINDS.get(usize::from(arg)).map(|&k| Form::Site(k)),
        (4, 0) => Some(Form::Block),
        (5, 0) => Some(Form::Slot),
        _ => None,
    }
}

fn write_pattern(out: &mut AVec<'_, u8>, p: &Pattern) {
    out.extend_from_slice(&[op_byte(p.op), p.width, p.variant]);
    out.extend_from_slice(&form_bytes(p.result));
    for &form in &p.operands {
        out.extend_from_slice(&form_bytes(form));
    }
    match p.inner {
        Some(inner) => out.extend_from_slice(&[1, op_byte(inner.op), inner.variant, inner.operand]),
        None => out.extend_from_slice(&[0, 0, 0, 0]),
    }
}

/// `Op` is `#[repr(u8)]`: its byte is the `ops` column's, which `Op::from_byte` reads back.
const fn op_byte(op: Op) -> u8 {
    op as u8
}

fn write_field(out: &mut AVec<'_, u8>, field: &Field) {
    let (kind, arg, extra) = match field.kind {
        FieldKind::Reg => (0, 0, 0),
        FieldKind::Imm => (1, 0, 0),
        FieldKind::Site(site) => (2, site_byte(site), 0),
        FieldKind::Branch { scale, from } => (3, scale, from),
        FieldKind::Frame { scale } => (4, scale, 0),
    };
    out.extend_from_slice(&field.at.to_le_bytes());
    out.extend_from_slice(&[field.width, field.operand, field.from_bit, kind, arg, extra]);
}

/// Little-endian reads that refuse past the end by naming what was being read.
struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'b [u8], TableError> {
        let end = self.at.checked_add(n).ok_or(TableError::Truncated { what })?;
        let got = self.bytes.get(self.at..end).ok_or(TableError::Truncated { what })?;
        self.at = end;
        Ok(got)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, TableError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, TableError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, TableError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, TableError> {
        let b = self.take(8, what)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
}

/// Where one row's template is in the decoded table.
#[derive(Clone, Copy, Debug)]
struct RowAt {
    bytes_at: usize,
    bytes_len: usize,
    /// An index in [`EncodedTable::fields`] (decoded), not in the record.
    fields_at: usize,
    fields_len: usize,
    clobbers: RegSet,
}

/// A table in piece 0's record form, checked once and read through [`Table`]. The template bytes
/// are borrowed from the record; the rows' patterns and the fields are decoded once into the
/// caller's allocator, since a [`Template`] hands out `&[Field]`.
pub struct EncodedTable<'b> {
    head: TableHead,
    bytes: &'b [u8],
    patterns: AVec<'b, Pattern>,
    rows: AVec<'b, RowAt>,
    fields: AVec<'b, Field>,
}

/// The row a field error names when the row's own pattern is malformed (piece 0's `TableError`
/// has no row variant; the field number `u32::MAX` says "the pattern").
pub const PATTERN_FIELD: u32 = u32::MAX;

impl<'b> EncodedTable<'b> {
    /// `bytes` checked: the magic, every count and range, the rows strictly sorted by pattern,
    /// every field inside its template and naming an operand. The head's digest is BLAKE3 of
    /// `bytes`; a caller holding a key compares it.
    pub fn read(bytes: &'b [u8], alloc: Alloc<'b>) -> Result<EncodedTable<'b>, TableError> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(TABLE_MAGIC.len(), "the magic")? != TABLE_MAGIC {
            return Err(TableError::Magic);
        }
        let arch = match r.u8("the arch")? {
            0 => Arch::Arm64,
            1 => Arch::X86_64,
            _ => return Err(TableError::Magic),
        };
        let os = match r.u8("the os")? {
            0 => Os::MacOs,
            1 => Os::Linux,
            _ => return Err(TableError::Magic),
        };
        let source = match r.u8("the source")? {
            0 => Source::Encoders,
            1 => Source::Llvm,
            _ => return Err(TableError::Magic),
        };
        r.u8("the head's pad")?;
        let features = r.u64("the features")?;
        let count = r.u32("the row count")?;
        let bytes_len = r.u32("the template bytes' length")? as usize;
        let fields_len = r.u32("the field count")? as usize;
        let rows_start = r.at;
        let rows_len = (count as usize).checked_mul(ROW_BYTES).ok_or(TableError::Truncated { what: "the rows" })?;
        r.take(rows_len, "the rows")?;
        let code = r.take(bytes_len, "the template bytes")?;
        let fields_bytes =
            r.take(fields_len.checked_mul(FIELD_BYTES).ok_or(TableError::Truncated { what: "the fields" })?, "the fields")?;
        if r.at != bytes.len() {
            return Err(TableError::Truncated { what: "the table's length (bytes after its fields)" });
        }

        let mut patterns: AVec<'b, Pattern> = Vec::with_capacity_in(count as usize, alloc);
        let mut rows: AVec<'b, RowAt> = Vec::with_capacity_in(count as usize, alloc);
        let mut fields: AVec<'b, Field> = Vec::with_capacity_in(fields_len, alloc);
        let mut rr = Reader { bytes, at: rows_start };
        for row in 0..count {
            let pattern = read_pattern(&mut rr, row)?;
            if patterns.last().is_some_and(|last| *last >= pattern) {
                return Err(TableError::Unsorted { row });
            }
            let at = rr.u32("a row's bytes_at")? as usize;
            let len = usize::from(rr.u16("a row's bytes_len")?);
            let f_at = rr.u32("a row's fields_at")? as usize;
            let f_len = usize::from(rr.u16("a row's fields_len")?);
            let clobbers = RegSet(rr.u64("a row's clobbers")?);
            if at.checked_add(len).is_none_or(|end| end > code.len()) {
                return Err(TableError::Truncated { what: "a row's template bytes" });
            }
            if f_at.checked_add(f_len).is_none_or(|end| end > fields_len) {
                return Err(TableError::Truncated { what: "a row's fields" });
            }
            let decoded_at = fields.len();
            for k in 0..f_len {
                let field_no = u32::try_from(k).unwrap_or(u32::MAX - 1);
                let raw = &fields_bytes[(f_at + k) * FIELD_BYTES..(f_at + k + 1) * FIELD_BYTES];
                let field = read_field(raw).ok_or(TableError::Field { row, field: field_no, what: "an unknown kind" })?;
                if usize::from(field.operand) >= OPERANDS {
                    return Err(TableError::Field { row, field: field_no, what: "names no operand" });
                }
                if usize::from(field.at) + usize::from(field.width) > len * 8 {
                    return Err(TableError::Field { row, field: field_no, what: "past the template's bits" });
                }
                fields.push(field);
            }
            patterns.push(pattern);
            rows.push(RowAt { bytes_at: at, bytes_len: len, fields_at: decoded_at, fields_len: f_len, clobbers });
        }
        let head = TableHead { machine: Machine { arch, os, features }, source, count, digest: blaze_blake3::hash(bytes) };
        Ok(EncodedTable { head, bytes: code, patterns, rows, fields })
    }

    /// Every pattern the table serves, sorted.
    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }
}

fn read_pattern(r: &mut Reader<'_>, row: u32) -> Result<Pattern, TableError> {
    let bad = TableError::Field { row, field: PATTERN_FIELD, what: "the row's pattern names an unknown operation or form" };
    let op = Op::from_byte(r.u8("a row's op")?).ok_or(bad)?;
    let width = r.u8("a row's width")?;
    let variant = r.u8("a row's variant")?;
    let form = |r: &mut Reader<'_>| -> Result<Form, TableError> {
        let kind = r.u8("a row's form")?;
        let arg = r.u8("a row's form")?;
        form_of(kind, arg).ok_or(bad)
    };
    let result = form(r)?;
    let operands = [form(r)?, form(r)?, form(r)?, form(r)?];
    let present = r.u8("a row's inner operation")?;
    let inner_op = r.u8("a row's inner operation")?;
    let inner_variant = r.u8("a row's inner operation")?;
    let inner_operand = r.u8("a row's inner operation")?;
    let inner = match present {
        0 => None,
        1 => Some(Inner { op: Op::from_byte(inner_op).ok_or(bad)?, variant: inner_variant, operand: inner_operand }),
        _ => return Err(bad),
    };
    Ok(Pattern { op, width, variant, result, operands, inner })
}

fn read_field(raw: &[u8]) -> Option<Field> {
    let at = u16::from_le_bytes([raw[0], raw[1]]);
    let (width, operand, from_bit, kind, arg, extra) = (raw[2], raw[3], raw[4], raw[5], raw[6], raw[7]);
    let kind = match (kind, arg, extra) {
        (0, 0, 0) => FieldKind::Reg,
        (1, 0, 0) => FieldKind::Imm,
        (2, site, 0) => FieldKind::Site(*SITE_KINDS.get(usize::from(site))?),
        (3, scale, from) => FieldKind::Branch { scale, from },
        (4, scale, 0) => FieldKind::Frame { scale },
        _ => return None,
    };
    Some(Field { at, width, operand, from_bit, kind })
}

impl Table for EncodedTable<'_> {
    fn head(&self) -> TableHead {
        self.head
    }

    fn find(&self, pattern: &Pattern) -> Option<Template<'_>> {
        let i = self.patterns.binary_search(pattern).ok()?;
        let row = self.rows.get(i)?;
        Some(Template {
            pattern: *pattern,
            bytes: self.bytes.get(row.bytes_at..row.bytes_at + row.bytes_len)?,
            fields: self.fields.get(row.fields_at..row.fields_at + row.fields_len)?,
            clobbers: row.clobbers,
        })
    }
}

/// The operand numbers of piece 0, re-exported for recipes.
pub(crate) const RES: u8 = RESULT;
pub(crate) const T0: u8 = TEMP0;
pub(crate) const T1: u8 = TEMP1;
