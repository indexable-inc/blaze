//! The platform's own unwind index of an image, from neutral rows (NATIVE.md 3.6): Mach-O's
//! `__TEXT,__unwind_info` and ELF's `.eh_frame_hdr`. The system unwinder finds both for a loaded
//! image by itself (`_dyld_find_unwind_sections`; `dl_iterate_phdr` and `PT_GNU_EH_FRAME`), so
//! nothing registers them. The rows are neutral so the export of Rust unwind tables (UNWIND.md
//! P10) writes its `__unwind_info` with the same writer.
//!
//! `__unwind_info` (`<mach-o/compact_unwind_encoding.h>`): a header
//! (`unwind_info_section_header`: version 1, the common encodings, the personalities, the
//! first-level index), the personality array (image offsets of pointer cells, at most three:
//! `UNWIND_PERSONALITY_MASK` is two bits, index 0 meaning none), the first-level index
//! (`unwind_info_section_header_index_entry`, one per second-level page and a last one whose
//! `functionOffset` is the end of the last function), the LSDA index
//! (`unwind_info_section_header_lsda_index_entry`, sorted by function), then the second-level
//! pages. This writer uses only regular pages (`UNWIND_SECOND_LEVEL_REGULAR`, 2: each entry a
//! function offset and its whole encoding) of at most 511 entries (a 4096-byte page, as ld64
//! sizes them); compressed pages are an optimization of size, not of meaning. A regular entry
//! has no length: the unwinder takes the next entry's start as its end, so a gap between one
//! row's end and the next row's start gets an entry of encoding 0 ("no unwind information"),
//! or the function in the gap would unwind with its neighbour's rule.
//!
//! `.eh_frame_hdr` (LSB 5.0, "The .eh_frame_hdr section"): version 1, `eh_frame_ptr_enc`
//! `DW_EH_PE_pcrel | DW_EH_PE_sdata4`, `fde_count_enc` `DW_EH_PE_udata4`, `table_enc`
//! `DW_EH_PE_datarel | DW_EH_PE_sdata4`, then the binary search table of (initial location, FDE
//! address) pairs, both relative to the header's start, sorted by initial location.
//!
//! The writers append with `extend_from_slice`; the caller reserves [`unwind_info_len`] or
//! [`eh_frame_hdr_len`] bytes first (with `try_reserve`, as `layout::lay` does), so an
//! allocation failure is the caller's typed error, not a panic here.

use blaze_own::AVec;

/// One function's unwind row, at image-relative addresses (base 0 is the image's header).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row { pub start: u64, pub length: u32, pub encoding: u32, pub personality: Option<u64> /* GOT cell address */, pub lsda: Option<u64> }

/// Why an unwind index could not be written. A start, end, cell or LSDA address past 4 GiB (the
/// format's offsets are 32-bit) is refused as `Unsorted { at }` naming it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnwindInfoError { TooManyPersonalities { most: u32 }, Unsorted { at: u64 }, BadFde { at: u64 } }

/// `UNWIND_PERSONALITY_MASK` (`0x30000000`) holds a 1-based index: three personalities at most.
pub const MOST_PERSONALITIES: u32 = 3;
const UNWIND_SECTION_VERSION: u32 = 1;
const UNWIND_SECOND_LEVEL_REGULAR: u32 = 2;
const UNWIND_PERSONALITY_MASK: u32 = 0x3000_0000;
const UNWIND_HAS_LSDA: u32 = 0x4000_0000;
const HEADER_LEN: usize = 7 * 4;
const INDEX_ENTRY_LEN: usize = 3 * 4;
const LSDA_ENTRY_LEN: usize = 2 * 4;
const PAGE_HEADER_LEN: usize = 4 + 2 + 2;
const PAGE_ENTRY_LEN: usize = 2 * 4;
/// Entries of one regular page: (4096 - 8) / 8.
const PAGE_ENTRIES: usize = 511;
const PAGE_LEN: usize = PAGE_HEADER_LEN + PAGE_ENTRIES * PAGE_ENTRY_LEN;

fn off32(at: u64) -> Result<u32, UnwindInfoError> {
    u32::try_from(at).map_err(|_| UnwindInfoError::Unsorted { at })
}

/// What the rows make: checked once, before anything is written.
struct Shape {
    personalities: [u32; MOST_PERSONALITIES as usize],
    count: u32,
    entries: usize,
    lsdas: usize,
    end: u32,
}

impl Shape {
    fn pages(&self) -> usize {
        self.entries.div_ceil(PAGE_ENTRIES)
    }

    fn lsda_base(&self) -> usize {
        HEADER_LEN + 4 * self.count as usize + INDEX_ENTRY_LEN * (self.pages() + 1)
    }

    fn pages_base(&self) -> usize {
        self.lsda_base() + LSDA_ENTRY_LEN * self.lsdas
    }

    fn len(&self) -> usize {
        let pages = self.pages();
        let last = self.entries - pages.saturating_sub(1) * PAGE_ENTRIES;
        match pages {
            0 => self.pages_base(),
            _ => self.pages_base() + (pages - 1) * PAGE_LEN + PAGE_HEADER_LEN + last * PAGE_ENTRY_LEN,
        }
    }

    /// The 1-based index of personality cell `cell`, as the encoding's two bits hold it.
    fn personality(&self, cell: u32) -> u32 {
        let mut i = 0;
        while i < self.count as usize {
            if self.personalities[i] == cell {
                return i as u32 + 1;
            }
            i += 1;
        }
        0
    }
}

fn shape(rows: &[Row]) -> Result<Shape, UnwindInfoError> {
    let mut s = Shape { personalities: [0; MOST_PERSONALITIES as usize], count: 0, entries: 0, lsdas: 0, end: 0 };
    let mut prev: Option<(u64, u64)> = None;
    for row in rows {
        if let Some((start, end)) = prev {
            if row.start <= start || row.start < end {
                return Err(UnwindInfoError::Unsorted { at: row.start });
            }
            if row.start > end {
                s.entries += 1;
            }
        }
        let end = row.start.checked_add(u64::from(row.length)).ok_or(UnwindInfoError::Unsorted { at: row.start })?;
        off32(row.start)?;
        off32(end)?;
        s.entries += 1;
        if let Some(cell) = row.personality {
            let cell = off32(cell)?;
            if s.personality(cell) == 0 {
                if s.count == MOST_PERSONALITIES {
                    return Err(UnwindInfoError::TooManyPersonalities { most: MOST_PERSONALITIES });
                }
                s.personalities[s.count as usize] = cell;
                s.count += 1;
            }
        }
        if let Some(lsda) = row.lsda {
            off32(lsda)?;
            s.lsdas += 1;
        }
        prev = Some((row.start, end));
    }
    s.end = match prev {
        Some((_, end)) => off32(end)?,
        None => 0,
    };
    Ok(s)
}

/// The entries of the second-level pages: every row, and an entry of encoding 0 at a row's end
/// when the next row starts later. Run only over rows `shape` accepted.
struct Entries<'r> {
    rows: &'r [Row],
    next: usize,
    prev_end: Option<u64>,
}

impl Iterator for Entries<'_> {
    type Item = (u32, Option<Row>);

    fn next(&mut self) -> Option<(u32, Option<Row>)> {
        let row = *self.rows.get(self.next)?;
        if let Some(end) = self.prev_end
            && row.start > end
        {
            self.prev_end = None;
            return Some((end as u32, None));
        }
        self.next += 1;
        self.prev_end = Some(row.start + u64::from(row.length));
        Some((row.start as u32, Some(row)))
    }
}

fn entries(rows: &[Row]) -> Entries<'_> {
    Entries { rows, next: 0, prev_end: None }
}

fn put32(out: &mut AVec<'_, u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// The bytes `unwind_info` appends for `rows`.
pub fn unwind_info_len(rows: &[Row]) -> Result<usize, UnwindInfoError> {
    Ok(shape(rows)?.len())
}

/// Appends Mach-O `__unwind_info` for `rows` (sorted by start, not overlapping) to `out`. A row's
/// personality and LSDA bits are set here from its `personality` and `lsda`; the rest of its
/// encoding is kept as given.
pub fn unwind_info<'a>(rows: &[Row], out: &mut AVec<'a, u8>) -> Result<(), UnwindInfoError> {
    let s = shape(rows)?;
    let pages = s.pages();
    let lsda_base = s.lsda_base();
    let pages_base = s.pages_base();
    // unwind_info_section_header
    put32(out, UNWIND_SECTION_VERSION);
    put32(out, HEADER_LEN as u32); // commonEncodingsArraySectionOffset
    put32(out, 0); // commonEncodingsArrayCount
    put32(out, HEADER_LEN as u32); // personalityArraySectionOffset
    put32(out, s.count);
    put32(out, (HEADER_LEN + 4 * s.count as usize) as u32); // indexSectionOffset
    put32(out, (pages + 1) as u32); // indexCount
    let mut i = 0;
    while i < s.count as usize {
        put32(out, s.personalities[i]);
        i += 1;
    }
    // The first-level index: a page's first function and the LSDA entries before it.
    let mut lsdas_before = 0usize;
    for (k, (start, row)) in entries(rows).enumerate() {
        if k % PAGE_ENTRIES == 0 {
            put32(out, start);
            put32(out, (pages_base + (k / PAGE_ENTRIES) * PAGE_LEN) as u32);
            put32(out, (lsda_base + LSDA_ENTRY_LEN * lsdas_before) as u32);
        }
        if row.is_some_and(|r| r.lsda.is_some()) {
            lsdas_before += 1;
        }
    }
    put32(out, s.end);
    put32(out, 0);
    put32(out, (lsda_base + LSDA_ENTRY_LEN * s.lsdas) as u32);
    // The LSDA index, in function order.
    for row in rows {
        if let Some(lsda) = row.lsda {
            put32(out, row.start as u32);
            put32(out, lsda as u32);
        }
    }
    // The regular second-level pages.
    for (k, (start, row)) in entries(rows).enumerate() {
        if k % PAGE_ENTRIES == 0 {
            let here = (s.entries - k).min(PAGE_ENTRIES);
            put32(out, UNWIND_SECOND_LEVEL_REGULAR);
            out.extend_from_slice(&(PAGE_HEADER_LEN as u16).to_le_bytes()); // entryPageOffset
            out.extend_from_slice(&(here as u16).to_le_bytes()); // entryCount
        }
        let encoding = match row {
            None => 0,
            Some(r) => {
                let personality = match r.personality {
                    Some(cell) => s.personality(cell as u32),
                    None => 0,
                };
                let lsda = if r.lsda.is_some() { UNWIND_HAS_LSDA } else { 0 };
                (r.encoding & !(UNWIND_PERSONALITY_MASK | UNWIND_HAS_LSDA)) | (personality << 28) | lsda
            }
        };
        put32(out, start);
        put32(out, encoding);
    }
    Ok(())
}

// ---- .eh_frame_hdr ----

const DW_EH_PE_ABSPTR: u8 = 0x00;
const DW_EH_PE_UDATA4: u8 = 0x03;
const DW_EH_PE_UDATA8: u8 = 0x04;
const DW_EH_PE_SDATA4: u8 = 0x0b;
const DW_EH_PE_SDATA8: u8 = 0x0c;
const DW_EH_PE_PCREL: u8 = 0x10;
const DW_EH_PE_DATAREL: u8 = 0x30;
const DW_EH_PE_OMIT: u8 = 0xff;
const HDR_LEN: usize = 12;

fn bad(eh_frame_at: u64, at: usize) -> UnwindInfoError {
    UnwindInfoError::BadFde { at: eh_frame_at.wrapping_add(at as u64) }
}

fn read32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    let mut w = [0u8; 8];
    w.copy_from_slice(s);
    Some(u64::from_le_bytes(w))
}

/// Skips one LEB128 value at `*at` (iterative; at most ten bytes).
fn skip_leb(b: &[u8], at: &mut usize) -> Option<()> {
    let mut n = 0;
    while n < 10 {
        let byte = *b.get(*at)?;
        *at += 1;
        if byte & 0x80 == 0 {
            return Some(());
        }
        n += 1;
    }
    None
}

fn read_uleb(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    while shift < 64 {
        let byte = *b.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

/// The size of a pointer of encoding `enc` (low four bits), or None for one this reader refuses.
fn pointer_size(enc: u8) -> Option<usize> {
    match enc & 0x0f {
        DW_EH_PE_ABSPTR | DW_EH_PE_UDATA8 | DW_EH_PE_SDATA8 => Some(8),
        DW_EH_PE_UDATA4 | DW_EH_PE_SDATA4 => Some(4),
        _ => None,
    }
}

/// The FDE pointer encoding (augmentation `R`) of the CIE at `cie` (LSB 5.0, "The Common
/// Information Entry Format"); `DW_EH_PE_absptr` when the augmentation has no `R`.
fn fde_encoding(b: &[u8], cie: usize) -> Option<u8> {
    let length = read32(b, cie)? as usize;
    let end = cie.checked_add(4)?.checked_add(length)?;
    if read32(b, cie + 4)? != 0 || end > b.len() {
        return None;
    }
    let mut at = cie + 8;
    let version = *b.get(at)?;
    at += 1;
    let aug_start = at;
    while *b.get(at)? != 0 {
        at += 1;
    }
    let aug = b.get(aug_start..at)?;
    at += 1;
    skip_leb(b, &mut at)?; // code alignment factor
    skip_leb(b, &mut at)?; // data alignment factor
    if version == 1 {
        at += 1; // return address register, a byte in version 1
    } else {
        skip_leb(b, &mut at)?;
    }
    if aug.is_empty() {
        return Some(DW_EH_PE_ABSPTR);
    }
    if aug[0] != b'z' {
        return None;
    }
    read_uleb(b, &mut at)?; // augmentation data length
    let mut i = 1;
    while i < aug.len() {
        match aug[i] {
            b'R' => return Some(*b.get(at)?),
            b'P' => {
                let enc = *b.get(at)?;
                at += 1;
                if enc & 0x70 > DW_EH_PE_PCREL {
                    return None;
                }
                at += pointer_size(enc)?;
            }
            b'L' => at += 1,
            b'S' | b'B' | b'G' => {}
            _ => return None,
        }
        i += 1;
    }
    Some(DW_EH_PE_ABSPTR)
}

/// The FDE's initial location, an image-relative address: its `pc_begin` read by `enc`.
fn pc_begin(b: &[u8], at: usize, enc: u8, eh_frame_at: u64) -> Option<u64> {
    let raw = match enc & 0x0f {
        DW_EH_PE_ABSPTR | DW_EH_PE_UDATA8 | DW_EH_PE_SDATA8 => read64(b, at)?,
        DW_EH_PE_UDATA4 => u64::from(read32(b, at)?),
        DW_EH_PE_SDATA4 => read32(b, at)? as i32 as i64 as u64,
        _ => return None,
    };
    match enc & 0xf0 {
        0 => Some(raw),
        DW_EH_PE_PCREL => Some(eh_frame_at.wrapping_add(at as u64).wrapping_add(raw)),
        _ => None,
    }
}

/// Walks the records of a relocated `.eh_frame`; calls `fde(at, initial location)` per FDE.
fn each_fde(eh_frame: &[u8], eh_frame_at: u64, mut fde: impl FnMut(usize, u64) -> Result<(), UnwindInfoError>) -> Result<(), UnwindInfoError> {
    let mut at = 0usize;
    while at < eh_frame.len() {
        let length = read32(eh_frame, at).ok_or(bad(eh_frame_at, at))?;
        if length == 0 {
            break; // the terminator
        }
        if length == u32::MAX {
            return Err(bad(eh_frame_at, at)); // 64-bit DWARF is not written by any aarch64 compiler
        }
        let end = at + 4 + length as usize;
        if end > eh_frame.len() {
            return Err(bad(eh_frame_at, at));
        }
        let id = read32(eh_frame, at + 4).ok_or(bad(eh_frame_at, at))?;
        if id != 0 {
            let cie = (at + 4).checked_sub(id as usize).ok_or(bad(eh_frame_at, at))?;
            let enc = fde_encoding(eh_frame, cie).ok_or(bad(eh_frame_at, at))?;
            if enc == DW_EH_PE_OMIT {
                return Err(bad(eh_frame_at, at));
            }
            let initial = pc_begin(eh_frame, at + 8, enc, eh_frame_at).ok_or(bad(eh_frame_at, at))?;
            fde(at, initial)?;
        }
        at = end;
    }
    Ok(())
}

/// The bytes `eh_frame_hdr` appends for `eh_frame`.
pub fn eh_frame_hdr_len(eh_frame: &[u8], eh_frame_at: u64) -> Result<usize, UnwindInfoError> {
    let mut count = 0usize;
    each_fde(eh_frame, eh_frame_at, |_, _| {
        count += 1;
        Ok(())
    })?;
    Ok(HDR_LEN + 8 * count)
}

/// Appends `.eh_frame_hdr` for the relocated `eh_frame` placed at `eh_frame_at`, the header to
/// be placed at `hdr_at` (both image-relative).
pub fn eh_frame_hdr<'a>(eh_frame: &[u8], eh_frame_at: u64, hdr_at: u64, out: &mut AVec<'a, u8>) -> Result<(), UnwindInfoError> {
    let rel = |value: u64, at: u64| -> Result<i32, UnwindInfoError> {
        i32::try_from(value.wrapping_sub(hdr_at) as i64).map_err(|_| UnwindInfoError::BadFde { at })
    };
    let start = out.len();
    out.extend_from_slice(&[1, DW_EH_PE_PCREL | DW_EH_PE_SDATA4, DW_EH_PE_UDATA4, DW_EH_PE_DATAREL | DW_EH_PE_SDATA4]);
    let ptr = i32::try_from(eh_frame_at.wrapping_sub(hdr_at + 4) as i64).map_err(|_| UnwindInfoError::BadFde { at: eh_frame_at })?;
    out.extend_from_slice(&ptr.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // the count, set below
    let mut count = 0u32;
    each_fde(eh_frame, eh_frame_at, |at, initial| {
        let fde_at = eh_frame_at + at as u64;
        out.extend_from_slice(&rel(initial, fde_at)?.to_le_bytes());
        out.extend_from_slice(&rel(fde_at, fde_at)?.to_le_bytes());
        count += 1;
        Ok(())
    })?;
    out[start + 8..start + 12].copy_from_slice(&count.to_le_bytes());
    let (table, _) = out[start + HDR_LEN..].as_chunks_mut::<8>();
    table.sort_unstable_by_key(|pair| (i32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]), i32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]])));
    Ok(())
}
