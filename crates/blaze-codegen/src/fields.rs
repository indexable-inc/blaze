//! The bits of code or data a relocation owns, and the invariant that keeps a record free of
//! addresses (docs/v2/plan/design/backend-next.md, (a)): every relocated field is stored as
//! zero, so a record's bytes are a function of its key alone, never of where a build put it or
//! its targets. A patch writes the field's bits ([`Field::mask`]) and nothing else; clearing
//! them ([`clear`]) gives back the stored bytes, whatever placement was patched in.
//!
//! The patcher is the launcher's (blaze-launch's `place::patch`): this module says which bits
//! are whose, and refuses a record that breaks the invariant ([`check`]). Relax keeps the same
//! invariant (its `link` and `link_with` patch fields emitted as zero); nothing here is taken
//! from its code.

use crate::{RelocKind, RelocsView};

/// Where a relocation is: in a function's code, or in data (a data object, a static's
/// value). Only wasm's fields differ between the two: an operand LEB in code, a plain word in
/// data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum In {
    Code,
    Data,
}

/// The bytes a relocation's field spans, from its `at`, and which of their bits it owns: bit
/// `i` of `mask` is bit `i % 8` of byte `i / 8`, the bytes read little-endian.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Field {
    /// 0 for a relocation that marks an instruction and patches nothing (`TlsDescCall`).
    pub len: u8,
    pub mask: u128,
}

/// AArch64 `ADRP`'s `immlo` (bits 29..31) and `immhi` (bits 5..24).
const ADRP: u128 = 0x60FF_FFE0;
/// AArch64 `ADD` (immediate) and `LDR`/`STR` (unsigned offset): `imm12`, bits 10..22.
const IMM12: u128 = 0x003F_FC00;
/// AArch64 `B`/`BL`: `imm26`, bits 0..26.
const IMM26: u128 = 0x03FF_FFFF;

/// The value bits of a padded LEB of `bytes` bytes: the low seven of each.
const fn leb(bytes: u8) -> u128 {
    let mut mask = 0u128;
    let mut at = 0;
    while at < bytes {
        mask |= 0x7fu128 << (8 * at as u32);
        at += 1;
    }
    mask
}

/// The whole of `bytes` bytes.
const fn whole(bytes: u8) -> u128 {
    (1u128 << (8 * bytes as u32)) - 1
}

impl Field {
    const fn of_bytes(len: u8) -> Field {
        Field { len, mask: whole(len) }
    }

    const fn insn(mask: u128) -> Field {
        Field { len: 4, mask }
    }

    /// The field of a relocation of `kind` in `place`.
    pub const fn of(kind: RelocKind, place: In) -> Field {
        match kind {
            RelocKind::Abs64 => Field::of_bytes(8),
            RelocKind::PcRel32
            | RelocKind::GotPcRel32
            | RelocKind::TlsOffset
            | RelocKind::TlsGd32
            | RelocKind::TlsGotTpOff32
            | RelocKind::TlvRel32
            | RelocKind::SectionRel32 => Field::of_bytes(4),
            RelocKind::Call26 => Field::insn(IMM26),
            RelocKind::AdrpPage21
            | RelocKind::GotPage21
            | RelocKind::TlvPage21
            | RelocKind::TlsDescPage21
            | RelocKind::TlsIePage21 => Field::insn(ADRP),
            RelocKind::PageOffset12
            | RelocKind::GotPageOffset12
            | RelocKind::TlvPageOffset12
            | RelocKind::TlsDescLd64Lo12
            | RelocKind::TlsDescAddLo12
            | RelocKind::TlsIeLo12
            | RelocKind::TpHi12
            | RelocKind::TpLo12 => Field::insn(IMM12),
            RelocKind::TlsDescCall => Field { len: 0, mask: 0 },
            RelocKind::WasmFunction | RelocKind::WasmTable | RelocKind::WasmMemory => match place {
                In::Code => Field { len: 5, mask: leb(5) },
                In::Data => Field::of_bytes(4),
            },
            RelocKind::WasmTable64 | RelocKind::WasmMemory64 => match place {
                In::Code => Field { len: 10, mask: leb(10) },
                In::Data => Field::of_bytes(8),
            },
        }
    }

    /// The field's bytes at `at` of `bytes`, little-endian; `None` past the end.
    fn read(self, bytes: &[u8], at: u32) -> Option<u128> {
        let start = usize::try_from(at).ok()?;
        let end = start.checked_add(usize::from(self.len))?;
        let span = bytes.get(start..end)?;
        Some(span.iter().rev().fold(0u128, |word, &byte| (word << 8) | u128::from(byte)))
    }

    fn write(self, bytes: &mut [u8], at: u32, word: u128) {
        let start = at as usize;
        for (i, byte) in bytes[start..start + usize::from(self.len)].iter_mut().enumerate() {
            *byte = (word >> (8 * i)) as u8;
        }
    }
}

/// A record that breaks the invariant, by its relocation's row and offset (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldError {
    /// The field holds nonzero bits: an address or an addend left in the bytes.
    Nonzero { row: usize, at: u32, kind: RelocKind, bits: u128 },
    /// The field runs past the end of the bytes.
    PastEnd { row: usize, at: u32, kind: RelocKind, len: usize },
    /// The field shares bytes with an earlier relocation's (row `earlier`).
    Overlap { row: usize, at: u32, earlier: usize },
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            FieldError::Nonzero { row, at, kind, bits } => write!(
                f,
                "relocation {row} ({kind:?} at {at}) has bits {bits:#x} in its field: a record's relocated fields are stored as zero (docs/v2/plan/design/backend-next.md, (a))"
            ),
            FieldError::PastEnd { row, at, kind, len } => write!(
                f,
                "relocation {row} ({kind:?} at {at}) has a field past the end of its {len} bytes"
            ),
            FieldError::Overlap { row, at, earlier } => write!(
                f,
                "relocation {row} at {at} shares bytes with relocation {earlier}'s field"
            ),
        }
    }
}

impl std::error::Error for FieldError {}

/// Refuses `bytes` unless every field of `relocs` (rows sorted by `at`, as a table keeps
/// them) is within the bytes, shares no byte with another and holds zero in its bits.
/// Allocates nothing.
pub fn check<T>(bytes: &[u8], relocs: RelocsView<'_, T>, place: In) -> Result<(), FieldError> {
    // The earlier field that reaches furthest: its row and end.
    let mut reach: Option<(usize, u64)> = None;
    for (row, it) in relocs.iter().enumerate() {
        let field = Field::of(it.kind, place);
        let Some(word) = field.read(bytes, it.at) else {
            return Err(FieldError::PastEnd { row, at: it.at, kind: it.kind, len: bytes.len() });
        };
        if field.len == 0 {
            continue;
        }
        if let Some((earlier, end)) = reach
            && end > u64::from(it.at)
        {
            return Err(FieldError::Overlap { row, at: it.at, earlier });
        }
        let bits = word & field.mask;
        if bits != 0 {
            return Err(FieldError::Nonzero { row, at: it.at, kind: it.kind, bits });
        }
        let end = u64::from(it.at) + u64::from(field.len);
        if reach.is_none_or(|(_, far)| end > far) {
            reach = Some((row, end));
        }
    }
    Ok(())
}

/// Zeroes the bits of every field of `relocs` in `bytes`, keeping the rest: the inverse of any
/// patch, so the bytes of a placed function cleared are its record's. Fields past the end are
/// refused as [`check`] does; nothing is written then.
pub fn clear<T>(bytes: &mut [u8], relocs: RelocsView<'_, T>, place: In) -> Result<(), FieldError> {
    for (row, it) in relocs.iter().enumerate() {
        if Field::of(it.kind, place).read(bytes, it.at).is_none() {
            return Err(FieldError::PastEnd { row, at: it.at, kind: it.kind, len: bytes.len() });
        }
    }
    for it in relocs.iter() {
        let field = Field::of(it.kind, place);
        let word = field.read(bytes, it.at).expect("every field was checked to be in the bytes");
        field.write(bytes, it.at, word & !field.mask);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
