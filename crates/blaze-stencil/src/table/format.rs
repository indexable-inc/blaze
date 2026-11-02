//! The stencil library's bytes: one little-endian blob per architecture variant, written by
//! the generator ([`Writer`]) and read in place by the patcher ([`Reader`]). Shared with the
//! generator (`build.rs` includes this file with `#[path]`); it uses nothing but `std`,
//! `blaze_ids::bits`, [`crate::shape`] and [`crate::enc`].
//!
//! ```text
//! Header   84 bytes: MAGIC, FORMAT, shape::VERSION, variant, counts, seed, faults, then id
//! Stencil  [stencils] 16 bytes: code offset u32, len u16, tail u8, holes u8, first hole u32,
//!                     first fault u24 | faults u8
//! Hole     [holes]     8 bytes: at u16, form u8, kind u8, addend i32
//! Fault    [faults]    u16: where an access of the op's memory starts
//! Pilots   [buckets]   u16, padded to 4 bytes
//! Slots    [slots]     8 bytes: key u32, stencil u32 (an empty slot: both u32::MAX)
//! Code     bytes, padded to 16
//! Rodata   [entries]  16 bytes each
//! ```
//!
//! Every column's offset follows from the counts, so the header holds no offsets to check.
//!
//! The index is a PTHash perfect hash from [`crate::shape::Shape::key`] to a stencil: key `k`
//! hashes to `h = hash(k, seed)`, its bucket is `range(h >> 32, buckets)`, and its slot is
//! `range(mix(h ^ pilot(bucket)), slots)`: three multiplies, one pilot load and one compare.
//!
//! # What the patcher writes in each hole
//!
//! A hole's field gets the value ELF would compute for the relocation LLVM made: `S + A` for
//! an absolute form, `S + A - P` for a PC-relative one ([`crate::enc`]), `A` being the hole's
//! `addend` and `P` the hole's place in the function. `S` is, by [`HoleKind`]:
//!
//! - `Slot0..2`, `Result0..1`: the slot's offset above the frame register (never negative).
//! - `Imm0..2`: the operand's immediate, sign-extended to 64 bits (for `iconst`/`fconst`, the
//!   constant's bits sign-extended from the type's width; for `aux` 0 it fits 32 bits signed).
//! - `Offset`: the access's offset, sign-extended (for the stack ops, the stack slot's frame
//!   offset plus it).
//! - `Next`, `Target`: the place of the code after the instruction, of the taken edge.
//! - `Trap`: the stub of the instruction's first trap code: the `trapz`/`trapnz`/
//!   `uadd_overflow_trap` code, `INTEGER_DIVISION_BY_ZERO` for the divisions and remainders,
//!   `BAD_CONVERSION_TO_INTEGER` for `fcvt_to_uint`/`fcvt_to_sint`.
//! - `TrapOverflow`: the stub of `INTEGER_OVERFLOW` (`sdiv`'s `MIN / -1`, `fcvt_to_*`'s
//!   out-of-range values).
//! - `Const`: the place of the instruction's 16-byte constant (16-aligned) in the function.
//! - `Rodata`: see [`Hole::rodata`]: the place of a 16-byte entry of the library's rodata,
//!   copied (16-aligned) once into the function that uses it.
//!
//! # Faults
//!
//! A stencil of a load, a store or an atomic lists ([`Stencil::faults`]) the offset of each
//! instruction that accesses the operation's memory, not its slots: where it can fault, which
//! the patcher records with the instruction's `MemFlags` trap code, as Cranelift records one
//! at each trapping access. An exclusive-pair loop has two.

use std::fmt;

use blaze_bytes::le::{u16 as u16_at, u32 as u32_at};
use blaze_ids::bits::BitSet;

use crate::{
    enc::Form,
    shape::{Op, Shape, Ty},
};

pub const MAGIC: [u8; 8] = *b"BZSTENC1";
/// Bumped with any change to this layout.
pub const FORMAT: u32 = 3;
pub const HEADER: usize = ID + 32;
/// Where the header's id starts.
const ID: usize = 52;
pub const STENCIL: usize = 16;
pub const HOLE: usize = 8;
pub const FAULT: usize = 2;
pub const SLOT: usize = 8;
/// Bytes of one rodata entry.
pub const ENTRY: usize = 16;
/// A slot no key hashes to.
pub const EMPTY: u32 = u32::MAX;

/// Which library: an architecture and the CPU baseline its stencils may use.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Variant {
    /// x86-64 baseline; `+cx16` only in the `i128` `atomic_cas` stencils.
    X86_64,
    /// AArch64 generic.
    Aarch64,
    /// AArch64 generic with `+lse` (Cranelift's `has_lse`, for `apple-m1`).
    Aarch64Lse,
}

impl Variant {
    pub const ALL: [Variant; 3] = [Variant::X86_64, Variant::Aarch64, Variant::Aarch64Lse];

    pub const fn from_u8(it: u8) -> Option<Variant> {
        if (it as usize) < Variant::ALL.len() { Some(Variant::ALL[it as usize]) } else { None }
    }

    /// The file the generator writes it to in `OUT_DIR`.
    pub const fn file(self) -> &'static str {
        match self {
            Variant::X86_64 => "x86_64.stencils",
            Variant::Aarch64 => "aarch64.stencils",
            Variant::Aarch64Lse => "aarch64_lse.stencils",
        }
    }

    /// The instruction set its stencils are of, which decides its shape set
    /// ([`crate::shape::each_on`]).
    pub const fn isa(self) -> crate::shape::Isa {
        match self {
            Variant::X86_64 => crate::shape::Isa::X86_64,
            Variant::Aarch64 | Variant::Aarch64Lse => crate::shape::Isa::Aarch64,
        }
    }

    /// Whether its library has a stencil of `shape`: its instruction set's set
    /// ([`Shape::exists_on`]) less an `i128` compare-and-swap without LSE, which Cranelift
    /// lowers only to `casp` (`isa/aarch64/lower.isle`, `lse_atomic_cas_i128`).
    pub fn has(self, shape: Shape) -> bool {
        shape.exists_on(self.isa())
            && !(self == Variant::Aarch64 && shape.op == Op::AtomicCas && shape.ty == Ty::I128)
    }
}

/// What a hole is filled with (stencil.md, "Holes"; the module docs say the value of each).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum HoleKind {
    /// The frame offset of operand 0's, 1's or 2's slot.
    Slot0,
    Slot1,
    Slot2,
    /// The frame offset of result 0's or 1's slot.
    Result0,
    Result1,
    /// Operand 0's, 1's or 2's immediate.
    Imm0,
    Imm1,
    Imm2,
    /// The memory access's offset; for the stack ops, the stack slot's frame offset plus it.
    Offset,
    /// The code after this instruction's: the jump to the continuation.
    Next,
    /// The taken branch's edge.
    Target,
    /// The trap stub of the instruction's trap code.
    Trap,
    /// The instruction's 16-byte constant in the function's constant area.
    Const,
    /// One 16-byte entry of the library's rodata (LLVM's constant pool): [`Hole::rodata`].
    Rodata,
    /// The trap stub of `INTEGER_OVERFLOW`, for the ops that trap with two codes.
    TrapOverflow,
}

impl HoleKind {
    pub const ALL: [HoleKind; 15] = [
        HoleKind::Slot0,
        HoleKind::Slot1,
        HoleKind::Slot2,
        HoleKind::Result0,
        HoleKind::Result1,
        HoleKind::Imm0,
        HoleKind::Imm1,
        HoleKind::Imm2,
        HoleKind::Offset,
        HoleKind::Next,
        HoleKind::Target,
        HoleKind::Trap,
        HoleKind::Const,
        HoleKind::Rodata,
        HoleKind::TrapOverflow,
    ];

    pub const fn from_u8(it: u8) -> Option<HoleKind> {
        if (it as usize) < HoleKind::ALL.len() { Some(HoleKind::ALL[it as usize]) } else { None }
    }

    /// The generator's name of its symbol (`hole.slot0`, ...), which the stencils' IR refers
    /// to and the object's relocations name. `Rodata` has none: its relocations name LLVM's
    /// constant pool sections.
    pub const fn symbol(self) -> &'static str {
        match self {
            HoleKind::Slot0 => "hole.slot0",
            HoleKind::Slot1 => "hole.slot1",
            HoleKind::Slot2 => "hole.slot2",
            HoleKind::Result0 => "hole.result0",
            HoleKind::Result1 => "hole.result1",
            HoleKind::Imm0 => "hole.imm0",
            HoleKind::Imm1 => "hole.imm1",
            HoleKind::Imm2 => "hole.imm2",
            HoleKind::Offset => "hole.offset",
            HoleKind::Next => "hole.next",
            HoleKind::Target => "hole.target",
            HoleKind::Trap => "hole.trap",
            HoleKind::Const => "hole.const",
            HoleKind::Rodata => "hole.rodata",
            HoleKind::TrapOverflow => "hole.trap_overflow",
        }
    }

    pub fn from_symbol(name: &str) -> Option<HoleKind> {
        HoleKind::ALL.into_iter().find(|it| *it != HoleKind::Rodata && it.symbol() == name)
    }

    /// Operand `n`'s slot hole.
    pub const fn slot(n: usize) -> HoleKind {
        [HoleKind::Slot0, HoleKind::Slot1, HoleKind::Slot2][n]
    }

    /// Result `k`'s slot hole.
    pub const fn result(k: usize) -> HoleKind {
        [HoleKind::Result0, HoleKind::Result1][k]
    }

    /// Operand `n`'s immediate hole.
    pub const fn imm(n: usize) -> HoleKind {
        [HoleKind::Imm0, HoleKind::Imm1, HoleKind::Imm2][n]
    }

    /// Whether its value is a place in the function, so its field is PC-relative; the others
    /// are numbers, so their fields are absolute.
    pub const fn is_place(self) -> bool {
        matches!(
            self,
            HoleKind::Next
                | HoleKind::Target
                | HoleKind::Trap
                | HoleKind::TrapOverflow
                | HoleKind::Const
                | HoleKind::Rodata
        )
    }
}

/// One hole: 8 bytes in the library.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hole {
    /// Offset in the stencil of its field (x86-64) or instruction (AArch64).
    pub at: u16,
    pub form: Form,
    pub kind: HoleKind,
    pub addend: i32,
}

impl Hole {
    /// A `Rodata` hole's entry and addend: its `addend` holds the entry's number in its high
    /// 24 bits and, in its low 8 as a signed byte, the addend to patch with, `S` being the
    /// place of the entry's copy in the function.
    pub const fn rodata(self) -> (u32, i64) {
        ((self.addend as u32) >> 8, self.addend as u8 as i8 as i64)
    }

    /// The `addend` of a `Rodata` hole of `entry` patched with `addend`; `None` when either
    /// does not fit.
    pub const fn rodata_addend(entry: u32, addend: i64) -> Option<i32> {
        if entry >= 1 << 24 || addend < i8::MIN as i64 || addend > i8::MAX as i64 {
            return None;
        }
        Some(((entry << 8) | (addend as i8 as u8 as u32)) as i32)
    }
}

/// One stencil, as the patcher reads it: borrowed from the library's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stencil<'a> {
    pub code: &'a [u8],
    /// Bytes at the end of `code` that are its jump to `Next`, left out when the next
    /// instruction's code follows directly; 0 when its last instruction is not that jump.
    /// The `Next` hole in those bytes is then not patched.
    pub tail: u8,
    holes: &'a [u8],
    faults: &'a [u8],
}

impl<'a> Stencil<'a> {
    pub fn holes(&self) -> impl ExactSizeIterator<Item = Hole> + 'a {
        let holes: &'a [u8] = self.holes;
        holes.chunks_exact(HOLE).map(decode_hole)
    }

    /// Where each access of the op's memory starts in `code` (the module docs' "Faults").
    pub fn faults(&self) -> impl ExactSizeIterator<Item = u16> + 'a {
        let faults: &'a [u8] = self.faults;
        faults.chunks_exact(FAULT).map(|it| u16::from_le_bytes([it[0], it[1]]))
    }

    /// Bytes copied when the next instruction follows directly.
    pub fn body(&self) -> &'a [u8] {
        let code: &'a [u8] = self.code;
        &code[..code.len() - usize::from(self.tail)]
    }
}

/// A stencil row's last word: its first fault (low 24 bits) and how many (high 8).
#[inline]
const fn fault_range(word: u32) -> (usize, usize) {
    ((word & 0xff_ffff) as usize, (word >> 24) as usize)
}

/// A hole from its 8 bytes, which [`Reader::new`] checked.
fn decode_hole(bytes: &[u8]) -> Hole {
    Hole {
        at: u16_at(bytes, 0),
        form: Form::ALL[usize::from(bytes[2])],
        kind: HoleKind::ALL[usize::from(bytes[3])],
        addend: u32_at(bytes, 4) as i32,
    }
}

fn encode_hole(hole: Hole, out: &mut Vec<u8>) {
    out.extend_from_slice(&hole.at.to_le_bytes());
    out.push(hole.form as u8);
    out.push(hole.kind as u8);
    out.extend_from_slice(&hole.addend.to_le_bytes());
}

/// Whether a hole of `kind` may be encoded in `form`: places in PC-relative fields, numbers
/// in absolute ones.
pub const fn fits(kind: HoleKind, form: Form) -> bool {
    kind.is_place() == form.pc_relative()
}

/// Why a library's bytes were refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatError {
    Magic,
    /// The format or shape version it was written with, and ours.
    Version { found: (u32, u32), ours: (u32, u32) },
    Variant(u8),
    /// A count or offset out of the bytes, or a hole out of its stencil, by what it is.
    Bounds(&'static str),
    Form(u8),
    Kind(u8),
    /// The writer was given a shape twice, or one [`Variant::has`] refuses.
    Shape(u32),
    /// The writer's index found no pilot for a bucket (it retries with more buckets first).
    Index,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Magic => f.write_str("not a stencil library"),
            FormatError::Version { found, ours } => write!(
                f,
                "a library of format {} and shapes {}, not {} and {}",
                found.0, found.1, ours.0, ours.1
            ),
            FormatError::Variant(it) => write!(f, "variant {it} is none"),
            FormatError::Bounds(what) => write!(f, "{what} is out of bounds"),
            FormatError::Form(it) => write!(f, "hole form {it} is none"),
            FormatError::Kind(it) => write!(f, "hole kind {it} is none"),
            FormatError::Shape(key) => match Shape::from_key(*key) {
                Some(shape) => write!(f, "shape {shape} twice or not of the set"),
                None => write!(f, "shape key {key:#x} is no shape"),
            },
            FormatError::Index => f.write_str("no perfect index was found"),
        }
    }
}

impl std::error::Error for FormatError {}

const K1: u64 = 0x9E37_79B9_7F4A_7C15;
const K2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const K3: u64 = 0x1656_67B1_9E37_79F9;

/// The key's hash under `seed`.
#[inline]
const fn hash(key: u32, seed: u32) -> u64 {
    let h = ((key as u64) | ((seed as u64) << 32)).wrapping_mul(K1);
    h ^ (h >> 29)
}

/// `x` scaled into `0..n` (Lemire's multiply-shift range reduction).
#[inline]
const fn range(x: u32, n: u32) -> u32 {
    ((x as u64 * n as u64) >> 32) as u32
}

#[inline]
const fn bucket(h: u64, buckets: u32) -> u32 {
    range((h >> 32) as u32, buckets)
}

#[inline]
const fn position(h: u64, pilot: u16, slots: u32) -> u32 {
    let x = (h ^ (pilot as u64).wrapping_mul(K2)).wrapping_mul(K3);
    range((x >> 32) as u32, slots)
}

/// Where each column starts, from the counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Layout {
    stencils: usize,
    holes: usize,
    faults: usize,
    pilots: usize,
    slots: usize,
    code: usize,
    rodata: usize,
    end: usize,
}

impl Layout {
    fn of(counts: &Counts) -> Option<Layout> {
        let stencils = HEADER;
        let holes = stencils.checked_add((counts.stencils as usize).checked_mul(STENCIL)?)?;
        let faults = holes.checked_add((counts.holes as usize).checked_mul(HOLE)?)?;
        let pilots = faults.checked_add((counts.faults as usize).checked_mul(FAULT)?)?;
        let slots = pilots.checked_add((counts.buckets as usize).checked_mul(2)?)?.next_multiple_of(4);
        let code = slots.checked_add((counts.slots as usize).checked_mul(SLOT)?)?;
        let rodata = code.checked_add(counts.code as usize)?.next_multiple_of(ENTRY);
        let end = rodata.checked_add((counts.entries as usize).checked_mul(ENTRY)?)?;
        Some(Layout { stencils, holes, faults, pilots, slots, code, rodata, end })
    }
}

/// The header's counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Counts {
    stencils: u32,
    holes: u32,
    buckets: u32,
    slots: u32,
    code: u32,
    entries: u32,
    seed: u32,
    faults: u32,
}

/// A library read in place: every count, offset and hole checked once by [`Reader::new`],
/// then read without checks beyond slice bounds.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    variant: Variant,
    counts: Counts,
    layout: Layout,
}

impl<'a> Reader<'a> {
    /// Checks the header (magic, [`FORMAT`], [`crate::shape::VERSION`]), that every column is
    /// in the bytes, that every hole is a known form and kind inside its stencil, that every
    /// key is a shape of the variant's set, and that the index finds every stencil once.
    pub fn new(bytes: &'a [u8]) -> Result<Reader<'a>, FormatError> {
        if bytes.len() < HEADER {
            return Err(FormatError::Bounds("the header"));
        }
        if bytes[..8] != MAGIC {
            return Err(FormatError::Magic);
        }
        let found = (u32_at(bytes, 8), u32_at(bytes, 12));
        let ours = (FORMAT, crate::shape::VERSION);
        if found != ours {
            return Err(FormatError::Version { found, ours });
        }
        let variant = Variant::from_u8(bytes[16]).ok_or(FormatError::Variant(bytes[16]))?;
        if bytes[17..20] != [0; 3] {
            return Err(FormatError::Bounds("the header's padding"));
        }
        let counts = Counts {
            stencils: u32_at(bytes, 20),
            holes: u32_at(bytes, 24),
            buckets: u32_at(bytes, 28),
            slots: u32_at(bytes, 32),
            code: u32_at(bytes, 36),
            entries: u32_at(bytes, 40),
            seed: u32_at(bytes, 44),
            faults: u32_at(bytes, 48),
        };
        let layout = Layout::of(&counts).ok_or(FormatError::Bounds("the columns"))?;
        if layout.end != bytes.len() {
            return Err(FormatError::Bounds("the columns"));
        }
        if counts.slots < counts.stencils || (counts.stencils > 0 && counts.buckets == 0) {
            return Err(FormatError::Bounds("the index"));
        }
        let reader = Reader { bytes, variant, counts, layout };
        reader.check_stencils()?;
        reader.check_index()?;
        Ok(reader)
    }

    fn check_stencils(&self) -> Result<(), FormatError> {
        let a64 = matches!(self.variant.isa(), crate::shape::Isa::Aarch64);
        for index in 0..self.counts.stencils {
            let row = self.layout.stencils + index as usize * STENCIL;
            let code = u32_at(self.bytes, row) as usize;
            let len = usize::from(u16_at(self.bytes, row + 4));
            let tail = usize::from(self.bytes[row + 6]);
            let holes = usize::from(self.bytes[row + 7]);
            let first = u32_at(self.bytes, row + 8) as usize;
            if code.checked_add(len).is_none_or(|end| end > self.counts.code as usize) {
                return Err(FormatError::Bounds("a stencil's code"));
            }
            if tail > len {
                return Err(FormatError::Bounds("a stencil's tail"));
            }
            if first.checked_add(holes).is_none_or(|end| end > self.counts.holes as usize) {
                return Err(FormatError::Bounds("a stencil's holes"));
            }
            let (first_fault, faults) = fault_range(u32_at(self.bytes, row + 12));
            if first_fault + faults > self.counts.faults as usize {
                return Err(FormatError::Bounds("a stencil's faults"));
            }
            for fault in first_fault..first_fault + faults {
                let at = usize::from(u16_at(self.bytes, self.layout.faults + fault * FAULT));
                if at >= len || (a64 && at % 4 != 0) {
                    return Err(FormatError::Bounds("a fault"));
                }
            }
            for hole in first..first + holes {
                let at = self.layout.holes + hole * HOLE;
                let form = Form::from_u8(self.bytes[at + 2])
                    .ok_or(FormatError::Form(self.bytes[at + 2]))?;
                let kind = HoleKind::from_u8(self.bytes[at + 3])
                    .ok_or(FormatError::Kind(self.bytes[at + 3]))?;
                let place = usize::from(u16_at(self.bytes, at));
                if place + usize::from(form.bytes()) > len {
                    return Err(FormatError::Bounds("a hole"));
                }
                if a64 != (form as u8 >= Form::A64MovwG0 as u8) || (a64 && place % 4 != 0) {
                    return Err(FormatError::Form(form as u8));
                }
                if !fits(kind, form) {
                    return Err(FormatError::Form(form as u8));
                }
                if kind == HoleKind::Rodata {
                    let hole = decode_hole(&self.bytes[at..at + HOLE]);
                    if hole.rodata().0 >= self.counts.entries {
                        return Err(FormatError::Bounds("a rodata hole's entry"));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_index(&self) -> Result<(), FormatError> {
        // alloc: once per library, at its first use
        let mut seen: BitSet<u32> = BitSet::new(self.counts.stencils as usize);
        let mut found = 0u32;
        for slot in 0..self.counts.slots {
            let at = self.layout.slots + slot as usize * SLOT;
            let key = u32_at(self.bytes, at);
            let stencil = u32_at(self.bytes, at + 4);
            if key == EMPTY {
                if stencil != EMPTY {
                    return Err(FormatError::Bounds("an empty slot"));
                }
                continue;
            }
            if stencil >= self.counts.stencils {
                return Err(FormatError::Bounds("a slot's stencil"));
            }
            if !seen.insert(stencil) {
                return Err(FormatError::Bounds("a stencil indexed twice"));
            }
            found += 1;
            if !Shape::from_key(key).is_some_and(|shape| self.variant.has(shape)) {
                return Err(FormatError::Shape(key));
            }
            if self.slot_of(key) != slot {
                return Err(FormatError::Index);
            }
        }
        if found != self.counts.stencils {
            return Err(FormatError::Bounds("a stencil not indexed"));
        }
        Ok(())
    }

    /// The slot `key` hashes to.
    #[inline]
    fn slot_of(&self, key: u32) -> u32 {
        let h = hash(key, self.counts.seed);
        let b = bucket(h, self.counts.buckets) as usize;
        let pilot = u16_at(self.bytes, self.layout.pilots + 2 * b);
        position(h, pilot, self.counts.slots)
    }

    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// The BLAKE3 hash of the bytes with this field zeroed, which the writer computed.
    pub fn id(&self) -> [u8; 32] {
        let mut id = [0; 32];
        id.copy_from_slice(&self.bytes[ID..HEADER]);
        id
    }

    pub fn len(&self) -> u32 {
        self.counts.stencils
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The stencil of `key`: three multiplies, a pilot and a compare.
    #[inline]
    pub fn find(&self, key: u32) -> Option<u32> {
        if self.counts.stencils == 0 {
            return None;
        }
        let at = self.layout.slots + self.slot_of(key) as usize * SLOT;
        if u32_at(self.bytes, at) == key { Some(u32_at(self.bytes, at + 4)) } else { None }
    }

    /// Stencil `index`, below [`Reader::len`].
    #[inline]
    pub fn stencil(&self, index: u32) -> Stencil<'a> {
        let bytes: &'a [u8] = self.bytes;
        let row = self.layout.stencils + index as usize * STENCIL;
        let code = self.layout.code + u32_at(bytes, row) as usize;
        let len = usize::from(u16_at(bytes, row + 4));
        let first = self.layout.holes + u32_at(bytes, row + 8) as usize * HOLE;
        let holes = usize::from(bytes[row + 7]) * HOLE;
        let (first_fault, faults) = fault_range(u32_at(bytes, row + 12));
        let first_fault = self.layout.faults + first_fault * FAULT;
        Stencil {
            code: &bytes[code..code + len],
            tail: bytes[row + 6],
            holes: &bytes[first..first + holes],
            faults: &bytes[first_fault..first_fault + faults * FAULT],
        }
    }

    /// The key stencil `index` was written for, from the index's slots (a scan: for tests and
    /// checks, not the patcher).
    pub fn key(&self, index: u32) -> u32 {
        (0..self.counts.slots)
            .map(|slot| self.layout.slots + slot as usize * SLOT)
            .find(|at| u32_at(self.bytes, at + 4) == index && u32_at(self.bytes, *at) != EMPTY)
            .map_or(EMPTY, |at| u32_at(self.bytes, at))
    }

    /// The library's read-only bytes: [`Reader::entries`] entries of [`ENTRY`] bytes.
    pub fn rodata(&self) -> &'a [u8] {
        let bytes: &'a [u8] = self.bytes;
        &bytes[self.layout.rodata..self.layout.end]
    }

    pub fn entries(&self) -> u32 {
        self.counts.entries
    }

    /// Rodata entry `entry` (a `Rodata` hole's, which [`Reader::new`] checked is below
    /// [`Reader::entries`]).
    #[inline]
    pub fn entry(&self, entry: u32) -> &'a [u8] {
        let at = entry as usize * ENTRY;
        &self.rodata()[at..at + ENTRY]
    }
}

/// Builds a library: the generator pushes each stencil in [`crate::shape::each_on`]'s order.
pub struct Writer {
    variant: Variant,
    keys: Vec<u32>,
    stencils: Vec<u8>,
    holes: Vec<u8>,
    code: Vec<u8>,
    rodata: Vec<u8>,
    hole_count: u32,
    faults: Vec<u8>,
}

impl Writer {
    pub fn new(variant: Variant) -> Writer {
        Writer {
            variant,
            keys: Vec::new(), // alloc: the generator, at blaze's build time
            stencils: Vec::new(), // alloc: the generator, at blaze's build time
            holes: Vec::new(), // alloc: the generator, at blaze's build time
            code: Vec::new(), // alloc: the generator, at blaze's build time
            rodata: Vec::new(), // alloc: the generator, at blaze's build time
            hole_count: 0,
            faults: Vec::new(), // alloc: the generator, at blaze's build time
        }
    }

    /// Adds the read-only bytes a stencil refers to (one constant of LLVM's constant pool,
    /// at most [`ENTRY`] bytes, aligned to at most [`ENTRY`]) as a rodata entry, or finds the
    /// equal entry added before; its number, for [`Hole::rodata_addend`].
    pub fn rodata(&mut self, bytes: &[u8], align: u32) -> Result<u32, FormatError> {
        if bytes.len() > ENTRY || align as usize > ENTRY {
            return Err(FormatError::Bounds("a rodata entry"));
        }
        let mut entry = [0u8; ENTRY];
        entry[..bytes.len()].copy_from_slice(bytes);
        let count = self.rodata.len() / ENTRY;
        let index = (0..count)
            .find(|i| self.rodata[i * ENTRY..(i + 1) * ENTRY] == entry)
            .unwrap_or_else(|| {
                self.rodata.extend_from_slice(&entry);
                count
            });
        Ok(index as u32)
    }

    /// Appends one stencil with no faults; its index.
    pub fn push(
        &mut self,
        shape: Shape,
        code: &[u8],
        tail: u8,
        holes: &[Hole],
    ) -> Result<u32, FormatError> {
        self.push_with_faults(shape, code, tail, holes, &[])
    }

    /// Appends one stencil, `faults` the offsets of its memory accesses (the module docs'
    /// "Faults"); its index.
    pub fn push_with_faults(
        &mut self,
        shape: Shape,
        code: &[u8],
        tail: u8,
        holes: &[Hole],
        faults: &[u16],
    ) -> Result<u32, FormatError> {
        let key = shape.key();
        if !self.variant.has(shape) || self.keys.contains(&key) {
            return Err(FormatError::Shape(key));
        }
        if code.len() > usize::from(u16::MAX) || usize::from(tail) > code.len() {
            return Err(FormatError::Bounds("a stencil's code"));
        }
        if holes.len() > usize::from(u8::MAX) {
            return Err(FormatError::Bounds("a stencil's holes"));
        }
        let entries = (self.rodata.len() / ENTRY) as u32;
        for hole in holes {
            if usize::from(hole.at) + usize::from(hole.form.bytes()) > code.len() {
                return Err(FormatError::Bounds("a hole"));
            }
            if !fits(hole.kind, hole.form) {
                return Err(FormatError::Form(hole.form as u8));
            }
            if hole.kind == HoleKind::Rodata && hole.rodata().0 >= entries {
                return Err(FormatError::Bounds("a rodata hole's entry"));
            }
        }
        let first_fault = self.faults.len() / FAULT;
        if faults.len() > usize::from(u8::MAX) || first_fault + faults.len() > 0xff_ffff {
            return Err(FormatError::Bounds("a stencil's faults"));
        }
        let a64 = matches!(self.variant.isa(), crate::shape::Isa::Aarch64);
        if faults.iter().any(|at| usize::from(*at) >= code.len() || (a64 && at % 4 != 0)) {
            return Err(FormatError::Bounds("a fault"));
        }
        let index = self.keys.len() as u32;
        let at = u32::try_from(self.code.len()).map_err(|_| FormatError::Bounds("the code"))?;
        self.stencils.extend_from_slice(&at.to_le_bytes());
        self.stencils.extend_from_slice(&(code.len() as u16).to_le_bytes());
        self.stencils.push(tail);
        self.stencils.push(holes.len() as u8);
        self.stencils.extend_from_slice(&self.hole_count.to_le_bytes());
        let fault_word = first_fault as u32 | (faults.len() as u32) << 24;
        self.stencils.extend_from_slice(&fault_word.to_le_bytes());
        for hole in holes {
            encode_hole(*hole, &mut self.holes);
        }
        for at in faults {
            self.faults.extend_from_slice(&at.to_le_bytes());
        }
        self.hole_count += holes.len() as u32;
        self.code.extend_from_slice(code);
        self.keys.push(key);
        Ok(index)
    }

    /// The library's bytes, with its perfect index and `hash` (BLAKE3, given by the caller,
    /// which has the dependency) of the bytes as its id.
    pub fn finish(self, hash: impl FnOnce(&[u8]) -> [u8; 32]) -> Result<Vec<u8>, FormatError> {
        let n = self.keys.len() as u32;
        let (seed, buckets, slots, pilots, table) = index(&self.keys)?;
        let counts = Counts {
            stencils: n,
            holes: self.hole_count,
            buckets,
            slots,
            code: u32::try_from(self.code.len()).map_err(|_| FormatError::Bounds("the code"))?,
            entries: (self.rodata.len() / ENTRY) as u32,
            seed,
            faults: (self.faults.len() / FAULT) as u32,
        };
        let layout = Layout::of(&counts).ok_or(FormatError::Bounds("the columns"))?;
        let mut out = Vec::with_capacity(layout.end);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&crate::shape::VERSION.to_le_bytes());
        out.extend_from_slice(&[self.variant as u8, 0, 0, 0]);
        let header =
            [counts.stencils, counts.holes, buckets, slots, counts.code, counts.entries, seed, counts.faults];
        for it in header {
            out.extend_from_slice(&it.to_le_bytes());
        }
        debug_assert_eq!(out.len(), ID);
        out.extend_from_slice(&[0; 32]);
        out.extend_from_slice(&self.stencils);
        out.extend_from_slice(&self.holes);
        out.extend_from_slice(&self.faults);
        for pilot in &pilots {
            out.extend_from_slice(&pilot.to_le_bytes());
        }
        out.resize(layout.slots, 0);
        for (key, stencil) in &table {
            out.extend_from_slice(&key.to_le_bytes());
            out.extend_from_slice(&stencil.to_le_bytes());
        }
        out.extend_from_slice(&self.code);
        out.resize(layout.rodata, 0);
        out.extend_from_slice(&self.rodata);
        debug_assert_eq!(out.len(), layout.end);
        let id = hash(&out);
        out[ID..HEADER].copy_from_slice(&id);
        Ok(out)
    }
}

/// The index's seed, bucket and slot counts, pilots and slots `(key, stencil)`, for `keys`
/// (distinct, stencil `i` of key `keys[i]`). PTHash's search: buckets by size, largest
/// first, each given the first pilot that puts all its keys in free slots; a bucket no pilot
/// places restarts the search with the next seed, then with more buckets.
#[allow(clippy::type_complexity)]
fn index(keys: &[u32]) -> Result<(u32, u32, u32, Vec<u16>, Vec<(u32, u32)>), FormatError> {
    let n = keys.len() as u32;
    if n == 0 {
        return Ok((0, 0, 0, Vec::new(), Vec::new())); // alloc: the generator, at blaze's build time
    }
    // Load factor about 0.95 and about 5 n / log2 n buckets: PTHash's usual point, where a
    // pilot is found in a few tries for most buckets.
    let slots = n + n / 19 + 1;
    let log = 32 - n.leading_zeros();
    for scale in [5u32, 7, 10, 16] {
        let buckets = (n * scale / log.max(1)).max(1);
        for seed in 0..32u32 {
            if let Some((pilots, table)) = place(keys, seed, buckets, slots) {
                return Ok((seed, buckets, slots, pilots, table));
            }
        }
    }
    Err(FormatError::Index)
}

#[allow(clippy::type_complexity)]
fn place(keys: &[u32], seed: u32, buckets: u32, slots: u32) -> Option<(Vec<u16>, Vec<(u32, u32)>)> {
    // alloc: the generator, at blaze's build time
    let mut members: Vec<Vec<(u64, u32)>> = vec![Vec::new(); buckets as usize];
    for (stencil, key) in keys.iter().enumerate() {
        let h = hash(*key, seed);
        members[bucket(h, buckets) as usize].push((h, stencil as u32));
    }
    let mut order: Vec<u32> = (0..buckets).collect(); // alloc: the generator, at blaze's build time
    order.sort_by_key(|b| (std::cmp::Reverse(members[*b as usize].len()), *b));
    let mut pilots = vec![0u16; buckets as usize]; // alloc: the generator, at blaze's build time
    // alloc: the generator, at blaze's build time
    let mut table = vec![(EMPTY, EMPTY); slots as usize];
    let mut taken = Vec::new(); // alloc: the generator, at blaze's build time
    for b in order {
        let bucket = &members[b as usize];
        if bucket.is_empty() {
            break;
        }
        let pilot = (0..=u16::MAX).find(|pilot| {
            taken.clear();
            for (h, _) in bucket {
                let at = position(*h, *pilot, slots);
                if table[at as usize].0 != EMPTY || taken.contains(&at) {
                    return false;
                }
                taken.push(at);
            }
            true
        })?;
        pilots[b as usize] = pilot;
        for (h, stencil) in bucket {
            table[position(*h, pilot, slots) as usize] = (keys[*stencil as usize], *stencil);
        }
    }
    Some((pilots, table))
}
