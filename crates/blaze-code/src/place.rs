//! [`place`]: one code record made code at an address in a running program (run.md, "End state",
//! "Slot table" and "Publish"; bottom.md section 11: "the launcher places them in the program's
//! own process behind one slot word a function").
//!
//! A run places each record alone, on its first call, into the run's code area. The record's
//! import word is the run's slot word: every `Load` and `Call` site is completed to reach the
//! word its target has in the run's one table ([`Words::word`]), and every relro word is filled
//! with its target's value ([`Words::value`]). The code area and the table are one reservation,
//! so every `adrp` reaches (4 GiB); a site that does not is refused by name, never relaxed.
//!
//! Pure: it writes the bytes of the run's write view that the caller hands it and nothing else.
//! The caller then does the cache maintenance and the word's release store (`Slots::publish`).
//! What a function's word and value are is the caller's (the launcher's control thread):
//!
//! - a call of a function loads its **slot word**: its stub until its first call, then its body;
//! - a load of a function's address (a `fn` pointer, a vtable's entry) loads, and a relro word
//!   holds, its **entry thunk**'s address, never a body: so a later tier or a freed body never
//!   leaves a pointer to stale code;
//! - a static, a vtable: a word holding its address; a thread-local: a word holding its index in
//!   the run's thread-local table; a C name: a word holding the address `dlsym` gave once.

use core::fmt;

use crate::a64::{ADD64, ADRP, BLR, LDR64, adrp, add_lo12, expect, ldr_lo12, write32};
use crate::image::ImageError;
use crate::record::{CodeView, OWN_CODE, OWN_RELRO, PositionPart, PositionRow, SiteKind, Space, Symbol, Target};

/// What the run's table answers for a record's targets. The control thread implements it over
/// its slot table; asking it a symbol first seen numbers a new slot whose word names its stub.
pub trait Words {
    /// The address of the 8-byte word a site of `target` loads. `call` is true for a
    /// [`SiteKind::Call`] (a function's slot word) and false for a [`SiteKind::Load`] (for a
    /// function, the word holding its entry thunk's address).
    fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError>;
    /// The value a relro word naming `target` holds, before its addend: a function's entry
    /// thunk, a static's or vtable's address, a thread-local's index in the run's thread-local
    /// table, a C name's address.
    fn value(&mut self, target: Target) -> Result<u64, ImageError>;
}

/// Where the record goes: its code's first byte and its relro part's, in the run view (the
/// addresses the program runs and loads at).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct At {
    pub code: u64,
    pub relro: u64,
}

/// Why a record was not placed, naming the function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaceError {
    /// The buffer for `what` is not the record's size.
    Size { symbol: Symbol, what: &'static str, want: usize, got: usize },
    /// `what` is not on the alignment its instructions or words need.
    Misaligned { symbol: Symbol, what: &'static str, at: u64, align: u64 },
    /// A parent's answers must give one little-endian u32 per sealed position row.
    PositionBytes { symbol: Symbol, rows: usize, bytes: usize },
    /// A site is not the instructions it says, cannot reach its word, or names nothing bound
    /// (record-offset positions).
    Site(ImageError),
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PlaceError::Size { symbol, what, want, got } => {
                write!(f, "function {symbol}: the {what} buffer is {got} bytes, the record's is {want}")
            }
            PlaceError::Misaligned { symbol, what, at, align } => {
                write!(f, "function {symbol}: {what} at {at:#x} is not {align}-aligned (run.md, \"Slot table\")")
            }
            PlaceError::PositionBytes { symbol, rows, bytes } => {
                write!(f, "function {symbol}: {rows} source position fields need one u32 each, received {bytes} bytes")
            }
            PlaceError::Site(error) => write!(f, "{error} (placing one record; offsets are the record's)"),
        }
    }
}

impl core::error::Error for PlaceError {}

impl From<ImageError> for PlaceError {
    fn from(error: ImageError) -> PlaceError {
        PlaceError::Site(error)
    }
}

/// Places `record` (function `symbol`) at `at`: copies its code into `code` and its relro part
/// into `relro` (the write view's bytes at `at.code` and `at.relro`), completes every site
/// against the run's words, and fills every relro word. Deterministic: the same record, `at` and
/// answers of `words` give the same bytes.
pub fn place(
    record: &CodeView<'_>,
    symbol: Symbol,
    at: At,
    words: &mut dyn Words,
    code: &mut [u8],
    relro: &mut [u8],
) -> Result<(), PlaceError> {
    place_positions(record, symbol, at, words, code, relro, |_, row| Err(ImageError::UnresolvedPosition { symbol, row }))
}

/// Places with the parent's transient answers, one little-endian u32 for each sealed row.
pub fn place_resolved(
    record: &CodeView<'_>,
    symbol: Symbol,
    at: At,
    words: &mut dyn Words,
    code: &mut [u8],
    relro: &mut [u8],
    values: &[u8],
) -> Result<(), PlaceError> {
    if record.positions().checked_mul(4) != Some(values.len()) {
        return Err(PlaceError::PositionBytes { symbol, rows: record.positions(), bytes: values.len() });
    }
    place_positions(record, symbol, at, words, code, relro, |i, _| Ok(crate::a64::read32(values, (i * 4) as u64)))
}

/// Places a record using this run's source-position answers in sealed row order. Only the
/// destination bytes receive the answers. A lookup failure prevents publication.
pub fn place_positions(
    record: &CodeView<'_>,
    symbol: Symbol,
    at: At,
    words: &mut dyn Words,
    code: &mut [u8],
    relro: &mut [u8],
    value: impl FnMut(usize, PositionRow) -> Result<u32, ImageError>,
) -> Result<(), PlaceError> {
    let want = record.code().len();
    if code.len() != want {
        return Err(PlaceError::Size { symbol, what: "code", want, got: code.len() });
    }
    let want = record.relro().len();
    if relro.len() != want {
        return Err(PlaceError::Size { symbol, what: "relro", want, got: relro.len() });
    }
    // Each part where its record says (record.rs `code_align`, `relro_align`): a data row
    // aligned to 64 is at a multiple of 64 from its part's first byte, so only that byte's
    // alignment makes the row's address one.
    let align = 1u64 << record.code_align();
    if at.code % align != 0 {
        return Err(PlaceError::Misaligned { symbol, what: "the code", at: at.code, align });
    }
    let align = 1u64 << record.relro_align();
    if at.relro % align != 0 {
        return Err(PlaceError::Misaligned { symbol, what: "the relro part", at: at.relro, align });
    }
    code.copy_from_slice(record.code());
    relro.copy_from_slice(record.relro());
    fill_positions(record, symbol, code, relro, value)?;
    for s in 0..record.sites() {
        let site = record.site(s);
        let off = u64::from(site.at);
        let pc = at.code + off;
        let first = expect(code, off, ADRP, symbol, "adrp")?;
        if site.kind == SiteKind::Relro {
            let second = expect(code, off + 4, ADD64, symbol, "add")?;
            let to = at.relro + u64::from(site.addend);
            write32(code, off, adrp(first, pc, to, symbol)?);
            write32(code, off + 4, add_lo12(second, to));
            continue;
        }
        let second = expect(code, off + 4, LDR64, symbol, "ldr")?;
        if site.kind == SiteKind::Call {
            expect(code, off + 8, BLR, symbol, "blr")?;
        }
        let target = record.target(site.target as usize);
        let word = words.word(target, site.kind == SiteKind::Call)?;
        if word % 8 != 0 {
            return Err(PlaceError::Misaligned { symbol, what: "a target's word", at: word, align: 8 });
        }
        write32(code, off, adrp(first, pc, word, symbol)?);
        write32(code, off + 4, ldr_lo12(second, word));
    }
    for w in 0..record.words() {
        let word = record.word(w);
        let value = match word.target {
            OWN_CODE => at.code,
            OWN_RELRO => at.relro,
            index => {
                let target = record.target(index as usize);
                if target.space == Space::Outside && word.addend != 0 {
                    return Err(PlaceError::Site(ImageError::OutsideAddend { symbol: target.symbol }));
                }
                words.value(target)?
            }
        };
        let filled = value.wrapping_add_signed(word.addend);
        let start = word.at as usize;
        relro[start..start + 8].copy_from_slice(&filled.to_le_bytes());
    }
    Ok(())
}

/// The slices start at this record's part bases. Its seal proves each field lies inside the
/// corresponding part and overlaps no site, address word or other field.
pub(crate) fn fill_positions(
    record: &CodeView<'_>,
    symbol: Symbol,
    code: &mut [u8],
    relro: &mut [u8],
    mut value: impl FnMut(usize, PositionRow) -> Result<u32, ImageError>,
) -> Result<(), ImageError> {
    for i in 0..record.positions() {
        let row = record.position(i);
        let dst = match row.part {
            PositionPart::Code => &mut *code,
            PositionPart::Relro => &mut *relro,
        };
        let resolved = value(i, row)?;
        if resolved == 0 {
            return Err(ImageError::ZeroPosition { symbol, row });
        }
        write32(dst, u64::from(row.at), resolved);
    }
    Ok(())
}
