//! The template table (D97 point 2, D99): the Fast tier's only source of machine code. A template
//! is the bytes of one operation for one shape of operands, with marked fields the composer fills
//! (registers, immediates, import sites, branch targets, frame offsets) and the registers it
//! clobbers. Two generators fill the same table in the same form: blaze-emit's encoders (each run
//! once with placeholder registers) and LLVM (the one LIR-to-LLVM lowering with operands pinned to
//! placeholder registers); where both give a template they are tested equal.
//!
//! THE RECORD FORM, the one both generators write (little-endian):
//! `TABLE_MAGIC`; `arch: u8, os: u8, source: u8, 0: u8`; `features: u64`; `count: u32`;
//! `bytes_len: u32`; `fields_len: u32`; then `count` rows of [`ROW_BYTES`] each, sorted by
//! [`Pattern`]'s order: the pattern (op u8, width u8, variant u8, result form 2 bytes, four operand
//! forms 2 bytes each, the inner operation as `present u8, op u8, variant u8, operand u8`),
//! `bytes_at: u32, bytes_len: u16, fields_at: u32, fields_len: u16`,
//! `clobbers: u64`; then the template bytes; then the fields ([`FIELD_BYTES`] each: `at: u16,
//! width: u8, operand: u8, from_bit: u8, kind: u8, arg: u8, 0: u8`). A form is two bytes: its
//! kind (0 none, 1 reg, 2 imm, 3 site, 4 block, 5 slot) and its argument (the register class,
//! the [`ImmClass`] byte, the `SiteKind` byte, 0). The table's digest is BLAKE3 of these bytes; a
//! record key holds it (point 6).
//!
//! Bit positions: `at` counts bits from the template's first byte, bit 0 the low bit of byte 0
//! (an arm64 word's bit `b` in its `k`th word is `32 * k + b`); a register field of an x86-64
//! template is split (REX.B and ModRM.rm are two fields of one operand: `from_bit` says which bits
//! of the operand's number each holds).

use core::fmt;

use blaze_code::record::SiteKind;
use blaze_lir::Op;

use crate::{Machine, RegClass, RegSet};

pub const TABLE_MAGIC: [u8; 8] = *b"bzttab01";
pub const ROW_BYTES: usize = 3 + 2 + 8 + 4 + 4 + 2 + 4 + 2 + 8;
pub const FIELD_BYTES: usize = 8;

/// Operand numbers a field names: inputs are 0..4, then the result, then two temporaries the
/// composer gives from the description's scratch set.
pub const RESULT: u8 = 4;
pub const TEMP0: u8 = 5;
pub const TEMP1: u8 = 6;

/// Which constants an immediate field can hold: the composer checks the value against its class
/// before it picks the template (a constant outside every class goes to a register).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum ImmClass {
    /// arm64 `add`/`sub` immediate: 0..4096.
    U12 = 0,
    /// arm64 `add`/`sub` immediate shifted by 12.
    U12Lsl12 = 1,
    /// arm64 logical immediate of a 32-bit operation, held encoded as `N:immr:imms`.
    Logical32 = 2,
    /// arm64 logical immediate of a 64-bit operation, held encoded as `N:immr:imms`.
    Logical64 = 3,
    /// A shift amount below 32.
    Shift5 = 4,
    /// A shift amount below 64.
    Shift6 = 5,
    /// arm64 `movz`/`movk` 16-bit piece.
    Mov16 = 6,
    /// arm64 unscaled load/store offset: -256..256.
    S9 = 7,
    /// arm64 scaled load/store offset of an access of `1 << scale` bytes: 0..4096 units.
    U12Scaled0 = 8,
    U12Scaled1 = 9,
    U12Scaled2 = 10,
    U12Scaled3 = 11,
    U12Scaled4 = 12,
    /// x86-64 sign-extended 8-bit immediate.
    S8 = 13,
    /// x86-64 sign-extended 32-bit immediate.
    S32 = 14,
    /// x86-64 64-bit immediate (`movabs`).
    I64 = 15,
}

/// The form of one operand a template serves.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Form {
    None,
    Reg(RegClass),
    Imm(ImmClass),
    /// An outside reference through an import site of this kind.
    Site(SiteKind),
    /// A block of the function (a branch target).
    Block,
    /// A stack slot or a cell, by its offset from `sp`.
    Slot,
}

/// What a template serves: one LIR operation at one width, one variant (a condition, a
/// conversion's kind, an atomic ordering: the operation's own byte, 0 where it has none), and the
/// forms of its result and operands. Ordered so a table is a sorted list.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pattern {
    pub op: Op,
    /// The operation's width in bits (1..=128); 0 where it has none.
    pub width: u8,
    pub variant: u8,
    pub result: Form,
    pub operands: [Form; 4],
    /// A fused inner operation (D100 point 6): the value of `operands[inner.operand]` is made by
    /// `inner.op` and has this one use (compare and branch, an address folded into a load or a
    /// store, a shifted operand). The inner operation's own operands are the template's operands
    /// after the outer ones, in order.
    pub inner: Option<Inner>,
}

/// The inner operation of a fused template: one level only.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Inner {
    pub op: Op,
    pub variant: u8,
    /// Which outer operand it makes.
    pub operand: u8,
}

/// What a field of a template holds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum FieldKind {
    /// A register's number (its `hw`).
    Reg,
    /// An immediate, already in its class's encoding.
    Imm,
    /// An import site of this kind: the site's own fields are completed by the segment's layout
    /// (blaze-code), the composer records the site at the template's offset.
    Site(SiteKind),
    /// A branch to a block: the displacement in units of `1 << scale` bytes from the template's
    /// byte `from` (arm64: scale 2, from the branch word).
    Branch { scale: u8, from: u8 },
    /// A frame offset from `sp` in units of `1 << scale` bytes.
    Frame { scale: u8 },
}

/// One marked field.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Field {
    /// The first bit (see the module's documentation).
    pub at: u16,
    pub width: u8,
    /// Which operand fills it: 0..4, [`RESULT`], [`TEMP0`], [`TEMP1`].
    pub operand: u8,
    /// The operand value's first bit this field holds.
    pub from_bit: u8,
    pub kind: FieldKind,
}

/// One template as the table hands it out: borrowed from the table's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Template<'t> {
    pub pattern: Pattern,
    pub bytes: &'t [u8],
    pub fields: &'t [Field],
    /// Registers the template writes besides its result (temporaries, flags are not registers).
    pub clobbers: RegSet,
}

/// Who generated a table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Source {
    /// blaze-emit's encoders, run with placeholder registers.
    Encoders = 0,
    /// LLVM through the one LIR-to-LLVM lowering.
    Llvm = 1,
}

/// A table's head: what a record key holds of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableHead {
    pub machine: Machine,
    pub source: Source,
    pub count: u32,
    /// BLAKE3 of the table's record bytes.
    pub digest: [u8; 32],
}

/// The trait the composer reads templates through, and nothing else (D99).
pub trait Table {
    fn head(&self) -> TableHead;
    /// The template serving `pattern`, if the table has one.
    fn find(&self, pattern: &Pattern) -> Option<Template<'_>>;
}

/// Why a table was refused or could not serve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableError {
    /// No template serves `pattern` (the composer names the instruction around it).
    Missing { pattern: Pattern },
    /// The bytes do not start with [`TABLE_MAGIC`].
    Magic,
    /// A count, offset or length is past the bytes.
    Truncated { what: &'static str },
    /// Rows are not sorted by pattern, or a pattern is there twice.
    Unsorted { row: u32 },
    /// Field `field` of row `row` is malformed: past the template's bits, or naming no operand.
    Field { row: u32, field: u32, what: &'static str },
    /// The digest the key expects is not the table's.
    Digest,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TableError::Missing { pattern } => {
                write!(f, "no template for `{}` at width {} variant {}", pattern.op.name(), pattern.width, pattern.variant)
            }
            TableError::Magic => f.write_str("not a template table (magic)"),
            TableError::Truncated { what } => write!(f, "the table is truncated at {what}"),
            TableError::Unsorted { row } => write!(f, "row {row} is out of order or repeated"),
            TableError::Field { row, field, what } => write!(f, "row {row} field {field}: {what}"),
            TableError::Digest => f.write_str("the table's digest is not the one the key holds"),
        }
    }
}

impl core::error::Error for TableError {}
