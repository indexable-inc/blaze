//! Call frame information: the `.eh_frame` records a panic unwinds by.
//!
//! Each function with [`blaze_codegen::Unwind::Dwarf`] gets an FDE, and the CIE its fields
//! name (one per distinct CIE, shared). Native objects' `.eh_frame` sections are split into
//! their records, each kept as long as the code it describes is. Records are 4-aligned and a
//! multiple of 4 long, so placed one after another they leave no gap an unwinder would read
//! as the terminator.

use std::ops::Range;

use blaze_format::cursor::{self, Put, Reader};
use blaze_graph::par;
use blaze_hash::HashMap;
use smallvec::SmallVec;

use crate::model::View;

/// A record's bytes and the places in it the core relocates.
#[cfg(test)]
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Record {
    pub bytes: Vec<u8>,
    pub fixups: Fields,
}

/// A record's relocated fields, at most three (an FDE's CIE pointer, function and LSDA),
/// kept inline: making a record allocates nothing but what its bytes outgrow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Fields {
    items: [(u64, Field); 3],
    len: u8,
}

impl Fields {
    const NONE: Fields = Fields { items: [(0, Field::CiePointer); 3], len: 0 };

    fn push(&mut self, at: u64, field: Field) {
        self.items[self.len as usize] = (at, field);
        self.len += 1;
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (u64, Field)> {
        self.items[..self.len as usize].iter()
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }
}

/// What a field of a record is relocated to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    /// An FDE's CIE pointer: from the field back to its CIE (`Delta32` of the record's own
    /// address + 4, minus the CIE's).
    CiePointer,
    /// An FDE's first address, PC-relative (`Rel32`) to the function.
    Function,
    /// An FDE's LSDA, PC-relative to the function's data.
    Lsda,
    /// A CIE's personality: PC-relative to a GOT cell holding the routine's address
    /// (`DW_EH_PE_indirect`).
    Personality,
}

const PCREL_SDATA4: u8 = 0x1b;
const INDIRECT_PCREL_SDATA4: u8 = 0x9b;

/// A CIE: version 1, augmentation `zR`, or `zPLR` for one with a personality.
#[cfg(test)]
pub(crate) fn cie(
    code_align: u32,
    data_align: i32,
    return_address: u16,
    instructions: &[u8],
    personality: bool,
) -> Result<Record, String> {
    let mut bytes = Vec::with_capacity(64);
    let fixups = cie_into(code_align, data_align, return_address, instructions, personality, &mut bytes)?;
    Ok(Record { bytes, fixups })
}

/// An FDE, as [`fde_into`] writes it.
#[cfg(test)]
pub(crate) fn fde(size: u64, instructions: &[u8], lsda: bool, has_lsda: bool) -> Result<Record, String> {
    let mut bytes = Vec::with_capacity(64);
    let fixups = fde_into(size, instructions, lsda, has_lsda, &mut bytes)?;
    Ok(Record { bytes, fixups })
}

/// Writes a CIE over `bytes` (version 1, augmentation `zR`, or `zPLR` for one with a
/// personality): its fields to relocate. A reused buffer allocates nothing.
pub(crate) fn cie_into(
    code_align: u32,
    data_align: i32,
    return_address: u16,
    instructions: &[u8],
    personality: bool,
    bytes: &mut Vec<u8>,
) -> Result<Fields, String> {
    let ra = u8::try_from(return_address)
        // alloc: error path
        .map_err(|_| format!("return address register {return_address} past CIE version 1's 255"))?;
    // Header 8, version 1, augmentation 6, alignments 5 + 10, register 1, augmentation data
    // 9, then the instructions and at most 7 of padding.
    bytes.clear();
    bytes.reserve(40 + instructions.len() + 7);
    bytes.extend_from_slice(&[0; 8]);
    bytes.push(1);
    bytes.extend_from_slice(if personality { b"zPLR\0" } else { b"zR\0" });
    uleb(bytes, u64::from(code_align));
    sleb(bytes, i64::from(data_align));
    bytes.push(ra);
    let mut fixups = Fields::NONE;
    if personality {
        uleb(bytes, 7);
        bytes.push(INDIRECT_PCREL_SDATA4);
        fixups.push(bytes.len() as u64, Field::Personality);
        bytes.extend_from_slice(&[0; 4]);
        bytes.push(PCREL_SDATA4);
        bytes.push(PCREL_SDATA4);
    } else {
        uleb(bytes, 1);
        bytes.push(PCREL_SDATA4);
    }
    bytes.extend_from_slice(instructions);
    finish(bytes)?;
    Ok(fixups)
}

/// Writes over `bytes` an FDE of a function `size` bytes long, its CIE pointer and
/// addresses left to fixups: its fields to relocate. `lsda` says whether its CIE has an `L`
/// augmentation (one with a personality does): then the field is there, relocated when
/// `has_lsda`, 0 (none) otherwise. A reused buffer allocates nothing.
pub(crate) fn fde_into(
    size: u64,
    instructions: &[u8],
    lsda: bool,
    has_lsda: bool,
    bytes: &mut Vec<u8>,
) -> Result<Fields, String> {
    let range = u32::try_from(size).map_err(|_| "a function past 4 GiB in an FDE")?;
    bytes.clear();
    bytes.reserve(16 + 1 + 4 + instructions.len() + 7);
    bytes.extend_from_slice(&[0; 12]);
    bytes.put_u32(range);
    let mut fixups = Fields::NONE;
    fixups.push(4, Field::CiePointer);
    fixups.push(8, Field::Function);
    if lsda {
        uleb(bytes, 4);
        if has_lsda {
            fixups.push(bytes.len() as u64, Field::Lsda);
        }
        bytes.extend_from_slice(&[0; 4]);
    } else {
        uleb(bytes, 0);
    }
    bytes.extend_from_slice(instructions);
    finish(bytes)?;
    Ok(fixups)
}

/// Pads with `DW_CFA_nop` to a multiple of 8 and writes the length.
fn finish(bytes: &mut Vec<u8>) -> Result<(), String> {
    bytes.resize(bytes.len().next_multiple_of(8), 0);
    let len = u32::try_from(bytes.len() - 4).map_err(|_| "a frame record past 4 GiB")?;
    if len == u32::MAX {
        // alloc: error path
        return Err("a frame record as long as the 64-bit length escape".to_owned());
    }
    bytes.patch_u32(0, len);
    Ok(())
}

pub(crate) use blaze_format::leb::{write_i64 as sleb, write_u64 as uleb};

/// A record of a native `.eh_frame` section.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Native {
    pub range: Range<usize>,
    /// `None` for a CIE; for an FDE, the start of its CIE in the section.
    pub cie: Option<usize>,
}

/// Splits a native `.eh_frame` into its records, stopping at a terminator. An FDE's CIE is
/// found among the records before it by binary search (they are met in order).
pub(crate) fn split(section: &[u8]) -> Result<Vec<Native>, String> {
    // Compilers' records are mostly 24 to 64 bytes.
    let mut records: Vec<Native> = Vec::with_capacity(section.len() / 32);
    let mut read = Reader::new(section);
    while read.left() >= 4 {
        let at = read.at();
        let len = read.u32()? as usize;
        if len == 0 {
            break;
        }
        if len == u32::MAX as usize {
            // alloc: error path
            return Err("an .eh_frame record with a 64-bit length".to_owned());
        }
        let Some(body) = read.take(len).ok().filter(|_| len >= 4) else {
            // alloc: error path
            return Err(format!("an .eh_frame record at {at:#x} past the section's end"));
        };
        let end = read.at();
        let id = Reader::new(body).u32()? as usize;
        let cie = match id {
            0 => None,
            back => Some(
                // alloc: error path
                (at + 4).checked_sub(back).ok_or_else(|| format!("an FDE at {at:#x} before its CIE"))?,
            ),
        };
        if let Some(cie) = cie {
            let found = records.binary_search_by_key(&cie, |it| it.range.start);
            if !found.is_ok_and(|it| records[it].cie.is_none()) {
                // alloc: error path
                return Err(format!("an FDE at {at:#x} whose CIE at {cie:#x} is no CIE before it"));
            }
        }
        records.push(Native { range: at..end, cie });
    }
    Ok(records)
}

pub(crate) const DW_EH_PE_OMIT: u8 = 0xff;
const DW_EH_PE_PCREL: u8 = 0x10;

/// The FDE pointer encoding of the CIE `reader` is in (past its ID), what leads to it checked
/// as gimli checks it. `reader`'s positions are from the start of the section at `section`.
pub(crate) fn cie_encoding(reader: &mut Reader<'_>, section: u64) -> Result<u8, &'static str> {
    const UNKNOWN: &str = "a CIE of an unknown augmentation";
    let version = reader.u8().map_err(cursor::Error::as_str)?;
    if !matches!(version, 1 | 3 | 4) {
        return Err("a CIE of an unknown version");
    }
    let augmentation = reader.cstr().map_err(cursor::Error::as_str)?;
    reader.uleb().map_err(cursor::Error::as_str)?;
    reader.sleb().map_err(cursor::Error::as_str)?;
    if version == 1 {
        reader.u8().map_err(cursor::Error::as_str)?;
    } else {
        reader.uleb().map_err(cursor::Error::as_str)?;
    }
    let mut encoding = 0;
    let mut data: Option<Reader<'_>> = None;
    for (index, &ch) in augmentation.iter().enumerate() {
        match ch {
            b'z' if index == 0 => {
                let len = reader.uleb().map_err(cursor::Error::as_str)?;
                let start = reader.at();
                usize::try_from(len)
                    .ok()
                    .and_then(|it| reader.skip(it).ok())
                    .ok_or("augmentation data longer than its CIE")?;
                let bytes = &reader.bytes()[..reader.at()];
                data = Some(Reader::starting_at(bytes, start).map_err(cursor::Error::as_str)?);
            }
            b'L' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
            }
            b'P' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                let personality = pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
                eh_pointer(data, personality, section)?;
            }
            b'R' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                encoding = pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
            }
            b'S' => {}
            _ => return Err(UNKNOWN),
        }
    }
    Ok(encoding)
}

/// `encoding` if it is a `DW_EH_PE_*` pointer encoding.
fn pointer_encoding(encoding: u8) -> Result<u8, &'static str> {
    let format = matches!(encoding & 0x0f, 0x00..=0x04 | 0x09..=0x0c);
    if encoding == DW_EH_PE_OMIT || (format && encoding & 0x70 <= 0x50) {
        Ok(encoding)
    } else {
        Err("an unknown pointer encoding")
    }
}

/// A value in `encoding`'s format, signed ones sign-extended.
pub(crate) fn eh_value(reader: &mut Reader<'_>, encoding: u8) -> Result<u64, &'static str> {
    let value = match encoding & 0x0f {
        0x00 | 0x04 | 0x0c => reader.u64(),
        0x01 => reader.uleb(),
        0x02 => reader.u16().map(u64::from),
        0x03 => reader.u32().map(u64::from),
        0x09 => reader.sleb().map(|it| it as u64),
        0x0a => reader.i16().map(|it| it as u64),
        0x0b => reader.i32().map(|it| it as u64),
        _ => return Err("an unknown pointer encoding"),
    };
    value.map_err(cursor::Error::as_str)
}

/// A pointer in `encoding` with only the section's address (`section`) known: absolute or
/// relative to where it is.
pub(crate) fn eh_pointer(reader: &mut Reader<'_>, encoding: u8, section: u64) -> Result<u64, &'static str> {
    if encoding == DW_EH_PE_OMIT {
        return Err("a pointer whose encoding says it is omitted");
    }
    let base = match encoding & 0x70 {
        0x00 => 0,
        DW_EH_PE_PCREL => section.wrapping_add(reader.at() as u64),
        _ => return Err("a pointer relative to a base other than where it is"),
    };
    Ok(base.wrapping_add(eh_value(reader, encoding)?))
}

/// The offsets in an image's `.eh_frame` (at `section`, `len` bytes) of every `every`th FDE
/// the core placed, sorted: record starts a walk of the section can begin at, so its pieces are
/// walked in parallel. None below two pieces' worth of functions.
pub(crate) fn fde_splits(placed: &crate::model::Placed<'_>, section: u64, len: usize, every: usize) -> Vec<usize> {
    if placed.functions.len() < 2 * every {
        // alloc: empty, which allocates nothing
        return Vec::new();
    }
    // alloc: once per image, one offset per `every` functions
    let mut splits = Vec::with_capacity(placed.functions.len() / every + 1);
    splits.extend(
        (placed.functions.iter())
            .filter_map(|it| it.fde?.checked_sub(section))
            .filter(|&it| it > 0 && it < len as u64)
            .step_by(every)
            .map(|it| it as usize),
    );
    splits.sort_unstable();
    splits.dedup();
    splits
}

/// A record of an image's `.eh_frame` as [`scan`] reads it: a CIE's FDE pointer encoding, or
/// the address an FDE covers from and how many bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Frame {
    Cie(u8),
    Fde { pc: u64, len: u64 },
}

/// What a read short of a record's fields says, as [`Reader`] says it.
pub(crate) const EARLY: &str = "bytes that end early";

const NO_CIE: &str = "its CIE pointer names no CIE";

/// The record at `at`: where its body (the CIE ID or pointer) starts and where it ends; none
/// for a zero length, which ends the section.
pub(crate) fn entry(bytes: View<'_>, at: usize) -> Result<Option<(usize, usize)>, &'static str> {
    let (length, body) = match bytes.u32(at).ok_or(EARLY)? {
        0 => return Ok(None),
        0xffff_fff0..=0xffff_fffe => return Err("a reserved initial length"),
        0xffff_ffff => (bytes.u64(at + 4).ok_or(EARLY)?, at + 12),
        length => (u64::from(length), at + 4),
    };
    let end = usize::try_from(length)
        .ok()
        .and_then(|it| body.checked_add(it))
        .filter(|it| *it <= bytes.len())
        .ok_or("longer than the section")?;
    Ok(Some((body, end)))
}

/// The record at `at` of `.eh_frame` (`bytes`, at address `section`), whose body starts at
/// `body` and which ends at `next` ([`entry`]), as gimli reads it: CIE versions 1, 3 and 4;
/// the `z`, `L`, `P`, `R` and `S` augmentations; `pc_begin` in the CIE's FDE pointer encoding,
/// absolute or relative to where it is, indirection ignored. Of an FDE only the CIE pointer,
/// `pc_begin` and `pc_range` are read, of a CIE only what leads to its pointer encoding.
/// `cie` gives the encoding of the CIE at the offset an FDE names, or why it names none. The
/// record is read in place, or put together in `across` where it crosses a page.
pub(crate) fn frame(
    bytes: View<'_>,
    at: usize,
    (body, next): (usize, usize),
    section: u64,
    across: &mut Vec<u8>,
    cie: impl FnOnce(usize) -> Result<u8, &'static str>,
) -> Result<Frame, &'static str> {
    // Positions in `reader` are from the record's start, at `base` in the address space.
    let base = section + at as u64;
    let held = bytes.read(at, next - at, across).ok_or(EARLY)?;
    let mut reader = Reader::starting_at(held, body - at).map_err(cursor::Error::as_str)?;
    let id = reader.u32().map_err(cursor::Error::as_str)?;
    if id == 0 {
        return cie_encoding(&mut reader, base).map(Frame::Cie);
    }
    let named = body.checked_sub(id as usize).ok_or("its CIE pointer is before the section")?;
    let encoding = cie(named)?;
    let pc = eh_pointer(&mut reader, encoding, base)?;
    let len = eh_value(&mut reader, encoding)?;
    Ok(Frame::Fde { pc, len })
}

/// The FDE pointer encoding of the CIE at `at`, which a walk of another piece met; `across`
/// holds it where it crosses a page. Any error is the FDE's naming no CIE: a CIE there that
/// reads wrong is an error of the piece holding it, which comes first.
fn cie_at(bytes: View<'_>, at: usize, section: u64, across: &mut Vec<u8>) -> Result<u8, &'static str> {
    let (body, end) = entry(bytes, at).ok().flatten().ok_or(NO_CIE)?;
    let held = bytes.read(at, end - at, across).ok_or(NO_CIE)?;
    let mut reader = Reader::starting_at(held, body - at).map_err(|_| NO_CIE)?;
    if reader.u32().map_err(|_| NO_CIE)? != 0 {
        return Err(NO_CIE);
    }
    cie_encoding(&mut reader, section + at as u64).map_err(|_| NO_CIE)
}

/// How a walk over a piece of `.eh_frame` ended.
enum Stop {
    /// At the piece's end, exactly.
    End,
    /// At a zero length, at this offset: the section ends here, whatever follows.
    Terminator(usize),
}

/// What a walk over a piece met besides what it kept: the CIEs in it, in order; the CIEs
/// before it its FDEs named, read where they are, each with its encoding and the first FDE
/// naming it (each must be one a walk of the pieces before met); and how it ended.
struct Walked {
    cies: SmallVec<[(usize, u8); 16]>,
    foreign: HashMap<usize, (u8, usize)>,
    stop: Result<Stop, String>,
}

/// The records from `start` (a record's start) up to `end`, each given to `each` with its
/// offset to keep what it needs onto `items`: an error when a record runs past `end` where
/// `end` is not the section's. An FDE's CIE pointer must name a CIE met before it: one in
/// this piece, in `known` (the CIEs before `start`, when a scan found them), or, without
/// `known`, one [`Walked::foreign`] lists for the caller to check.
fn walk<T, F: Fn(usize, Frame, &mut Vec<T>) -> Result<(), String>>(
    bytes: View<'_>,
    section: u64,
    (start, end): (usize, usize),
    known: Option<&[(usize, u8)]>,
    each: &F,
    items: &mut Vec<T>,
) -> Walked {
    let mut walked = Walked {
        // alloc: past 16 CIEs in one piece (one per native object), growing by doubling
        cies: SmallVec::new(),
        // alloc: only for a CIE another piece holds, once each
        foreign: HashMap::default(),
        stop: Ok(Stop::End),
    };
    // The last CIE an FDE named: most FDEs name the one before's.
    let mut last: Option<(usize, u8)> = None;
    // alloc: only for a record across two pages, grown to the longest such record once per walk
    let (mut across, mut cie_across) = (Vec::new(), Vec::new());
    // alloc: error path
    let bad = |at: usize, what: &str| format!("the entry at offset {at:#x}: {what}");
    let mut at = start;
    while at < end {
        let record = at;
        let found = match entry(bytes, record) {
            Ok(Some(found)) => found,
            Ok(None) => {
                walked.stop = Ok(Stop::Terminator(record));
                return walked;
            }
            Err(why) => {
                walked.stop = Err(bad(record, why));
                return walked;
            }
        };
        if found.1 > end {
            // alloc: error path
            walked.stop = Err(format!("the record at offset {record:#x} runs past {end:#x}, where the core placed an FDE"));
            return walked;
        }
        at = found.1;
        let Walked { cies, foreign, .. } = &mut walked;
        let lookup = |named: usize| -> Result<u8, &'static str> {
            if let Some((offset, encoding)) = last
                && offset == named
            {
                return Ok(encoding);
            }
            let encoding = if named >= start {
                let found = cies.binary_search_by_key(&named, |it| it.0).map_err(|_| NO_CIE)?;
                cies[found].1
            } else if let Some(known) = known {
                let found = known.binary_search_by_key(&named, |it| it.0).map_err(|_| NO_CIE)?;
                known[found].1
            } else if let Some(&(encoding, _)) = foreign.get(&named) {
                encoding
            } else {
                let encoding = cie_at(bytes, named, section, &mut cie_across)?;
                foreign.insert(named, (encoding, record));
                encoding
            };
            last = Some((named, encoding));
            Ok(encoding)
        };
        let frame = match frame(bytes, record, found, section, &mut across, lookup) {
            Ok(frame) => frame,
            Err(why) => {
                walked.stop = Err(bad(record, why));
                return walked;
            }
        };
        if let Frame::Cie(encoding) = frame {
            walked.cies.push((record, encoding));
        }
        if let Err(why) = each(record, frame, items) {
            walked.stop = Err(why);
            return walked;
        }
    }
    walked
}

/// What [`scan`] read: what `each` kept of the records, in section order; the CIEs, (offset,
/// FDE pointer encoding) in order; and where the walk stopped (a zero length's offset, or the
/// section's end).
pub(crate) struct Scan<T> {
    pub items: Vec<T>,
    pub cies: Vec<(usize, u8)>,
    pub end: usize,
}

/// Every record of an image's `.eh_frame` (`bytes`, at address `section`), given in order to
/// `each` with its offset ([`frame`] says how it is read), the walk ending at a zero length.
/// An FDE's CIE pointer must name a CIE before it, as [`split`] holds every native section to.
///
/// The section is walked in pieces from 0 and from each of `splits` (record starts, sorted:
/// [`fde_splits`]), in parallel; each piece must end exactly where the next starts. The answer
/// is the one walk from 0 gives, its first error included, on any number of cores. An error
/// `each` returns is the walk's at that record.
pub(crate) fn scan<T: Copy + Default + Send + Sync>(
    bytes: View<'_>,
    section: u64,
    splits: &[usize],
    each: impl Fn(usize, Frame, &mut Vec<T>) -> Result<(), String> + Sync,
) -> Result<Scan<T>, String> {
    let piece = |nth: usize| {
        let start = if nth == 0 { 0 } else { splits[nth - 1] };
        let end = splits.get(nth).copied().unwrap_or(bytes.len());
        // alloc: once per piece: compilers' records take 32 bytes or so, so it grows at most a
        // few times
        let mut items = Vec::with_capacity((end - start) / 32);
        let walked = walk(bytes, section, (start, end), None, &each, &mut items);
        (items, walked)
    };
    if splits.is_empty() {
        let (items, walked) = piece(0);
        let end = match walked.stop? {
            Stop::End => bytes.len(),
            Stop::Terminator(at) => at,
        };
        return Ok(Scan { items, cies: walked.cies.into_vec(), end });
    }
    // alloc: once per image, one result per piece
    let mut pieces: Vec<(Vec<T>, Walked)> = par::map(0..splits.len() + 1, piece);
    // The pieces the one walk reaches: up to the first that ends the section or fails. A piece's
    // first FDE naming no CIE met before is its first error: the walk went on past it.
    // alloc: once per image, the CIEs
    let mut cies: Vec<(usize, u8)> = Vec::with_capacity(pieces.iter().map(|it| it.1.cies.len()).sum());
    let (mut kept, mut end) = (pieces.len(), bytes.len());
    for (nth, (_, walked)) in pieces.iter_mut().enumerate() {
        let met = |named: &usize| cies.binary_search_by_key(named, |it| it.0).is_ok();
        let unmet = walked.foreign.iter().filter(|(named, _)| !met(named)).map(|(_, &(_, record))| record).min();
        if let Some(record) = unmet {
            // alloc: error path
            return Err(format!("the entry at offset {record:#x}: {NO_CIE}"));
        }
        cies.extend_from_slice(&walked.cies);
        match &mut walked.stop {
            Ok(Stop::End) => {}
            Ok(Stop::Terminator(at)) => {
                (kept, end) = (nth + 1, *at);
                break;
            }
            Err(why) => return Err(std::mem::take(why)),
        }
    }
    // alloc: once per image, one slice per piece
    let parts: Vec<&[T]> = pieces[..kept].iter().map(|it| &it.0[..]).collect();
    Ok(Scan { items: concat(&parts), cies, end })
}

/// `parts` one after another, each copied in parallel at its place.
pub(crate) fn concat<T: Copy + Default + Send + Sync>(parts: &[&[T]]) -> Vec<T> {
    let len = parts.iter().map(|it| it.len()).sum();
    // alloc: once per table, its exact length
    let mut items: Vec<T> = Vec::with_capacity(len);
    par::extend(&mut items, 0..len, |_| T::default());
    let mut rest = &mut items[..];
    // alloc: once per table, one slice per part
    let mut jobs = Vec::with_capacity(parts.len());
    for &part in parts {
        let (these, after) = std::mem::take(&mut rest).split_at_mut(part.len());
        jobs.push((these, part));
        rest = after;
    }
    par::for_each(jobs, |(out, part)| out.copy_from_slice(part));
    items
}

/// The records from `start` (a record's start) up to `end` given to `each` onto `items`, the
/// CIEs before `start` found in `cies` (a [`scan`]'s): what a scan's walk from `start` reads,
/// for a writer patching what it read last.
pub(crate) fn rewalk<T>(
    bytes: View<'_>,
    section: u64,
    (start, end): (usize, usize),
    cies: &[(usize, u8)],
    each: impl Fn(usize, Frame, &mut Vec<T>) -> Result<(), String>,
    items: &mut Vec<T>,
) -> Result<(), String> {
    walk(bytes, section, (start, end), Some(cies), &each, items).stop.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_padded_and_split_back() {
        let cie = cie(1, -8, 16, &[0x0c, 7, 8, 0x90, 1], true).unwrap();
        assert_eq!(cie.bytes.len() % 8, 0);
        assert_eq!(&cie.bytes[8..14], b"\x01zPLR\0");
        let fde = fde(0x40, &[0x41, 0x0e, 0x10], true, false).unwrap();
        assert_eq!(fde.bytes.len() % 8, 0);
        assert_eq!(u32::from_le_bytes(fde.bytes[12..16].try_into().unwrap()), 0x40);
        assert!(!fde.fixups.iter().any(|it| it.1 == Field::Lsda));

        let mut section = cie.bytes.clone();
        let mut first = fde.bytes.clone();
        let back = (section.len() + 4) as u32;
        first[4..8].copy_from_slice(&back.to_le_bytes());
        section.extend_from_slice(&first);
        section.extend_from_slice(&[0; 4]);
        let records = split(&section).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], Native { range: 0..cie.bytes.len(), cie: None });
        assert_eq!(records[1].cie, Some(0));
    }

    #[test]
    fn an_fde_without_its_cie_is_refused() {
        let mut fde = fde(4, &[], false, false).unwrap().bytes;
        fde[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert!(split(&fde).is_err());
    }

    /// Many CIEs, each FDE naming the latest one: the binary search finds every CIE, and an
    /// FDE pointing at another FDE is refused.
    #[test]
    fn fdes_find_their_cies_among_many() {
        let mut section = Vec::new();
        let mut want = Vec::new();
        for n in 0..50u64 {
            let start = section.len();
            section.extend_from_slice(&cie(1, -8, 16, &[], false).unwrap().bytes);
            want.push(None);
            for _ in 0..3 {
                let mut record = fde(n + 1, &[], false, false).unwrap();
                assert_eq!(record.fixups.len(), 2);
                let back = (section.len() + 4 - start) as u32;
                record.bytes[4..8].copy_from_slice(&back.to_le_bytes());
                section.extend_from_slice(&record.bytes);
                want.push(Some(start));
            }
        }
        let records = split(&section).unwrap();
        assert_eq!(records.iter().map(|it| it.cie).collect::<Vec<_>>(), want);

        let mut bad = cie(1, -8, 16, &[], false).unwrap().bytes;
        let first = bad.len();
        let mut fde_one = fde(4, &[], false, false).unwrap().bytes;
        fde_one[4..8].copy_from_slice(&((first + 4) as u32).to_le_bytes());
        bad.extend_from_slice(&fde_one);
        let mut fde_two = fde(4, &[], false, false).unwrap().bytes;
        fde_two[4..8].copy_from_slice(&((bad.len() + 4 - first) as u32).to_le_bytes());
        bad.extend_from_slice(&fde_two);
        assert!(split(&bad).unwrap_err().contains("no CIE"));
    }
}
