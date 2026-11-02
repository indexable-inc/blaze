//! `__unwind_info`: the table libunwind and dyld (`_dyld_find_unwind_sections`) look a pc up
//! in, in ld64's format (`<mach-o/compact_unwind_encoding.h>`), version 1.
//!
//! Every function with an FDE in `__eh_frame` gets the DWARF-mode encoding pointing at it;
//! every `__compact_unwind` entry gets its own encoding, with its personality and LSDA;
//! every other function, and the first address after a covered range where nothing starts,
//! gets encoding 0.
//!
//! Built as lld's `UnwindInfoSection` builds it: the FDEs from one scan of `__eh_frame`, then
//! one merge of four sorted columns (compact entries, FDEs, functions, ends of covered ranges)
//! into the entries; a second-level page is compressed (4-byte entries indexing the common
//! encodings or the page's own, as ld64 writes) unless a regular page holds more entries.
//!
//! An image linked again in place ([`build_kept`]) keeps the columns, rows and pages of its last
//! table ([`Last`]) and, when every change keeps each row in its place, rewrites only the rows,
//! LSDA entries, pages and first-level entries the change touched: O(change) but for the copy
//! of the table returned.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use blaze_bytes::le::{u32 as u32_at, u64 as u64_at};
use blaze_codegen::Arch;
use blaze_graph::par;
use blaze_ids::radix;

use crate::LinkError;
use crate::cow::CowBytes;
use crate::model::{Bind, Class, Delta, Function, Placed, Region, View};
use crate::unwind::{Frame, concat, entry, fde_splits, frame, scan};

const VERSION: u32 = 1;
const HEADER: u64 = 28;
const COMMON_MAX: usize = 127;
/// Encoding indexes of a compressed entry are 8 bits: the common ones, then the page's own.
const ENCODINGS_MAX: usize = 256;
const PERSONALITY_MAX: usize = 3;
const PAGE: u64 = 4096;
const PAGE_HEADER: u64 = 8;
const PAGE_ENTRY: u64 = 8;
const PAGE_ENTRIES: usize = ((PAGE - PAGE_HEADER) / PAGE_ENTRY) as usize;
const COMPRESSED_HEADER: u64 = 12;
const COMPRESSED_ENTRY: u64 = 4;
const COMPRESSED_OFFSET_MASK: u32 = 0x00FF_FFFF;
const SECOND_LEVEL_REGULAR: u32 = 2;
const SECOND_LEVEL_COMPRESSED: u32 = 3;
const INDEX_ENTRY: u64 = 12;
const LSDA_ENTRY: u64 = 8;
const COMPACT_ENTRY: u64 = 32;

const MODE_MASK: u32 = 0x0F00_0000;
const DWARF_OFFSET_MASK: u32 = 0x00FF_FFFF;
const HAS_LSDA: u32 = 0x4000_0000;
const PERSONALITY_MASK: u32 = 0x3000_0000;
const PERSONALITY_SHIFT: u32 = 28;
const ARM64_MODE_DWARF: u32 = 0x0300_0000;
const X86_64_MODE_DWARF: u32 = 0x0400_0000;

/// The most bytes `__unwind_info` can take for `functions` functions, `fdes` FDEs in
/// `__eh_frame` (a native object's may be of a function no other entry covers) and `compact`
/// bytes of `__compact_unwind` input (32 bytes an entry): the region is sized by it before
/// placement.
///
/// Counted in regular pages: every page but the last holds at least a regular page's 511
/// entries in at most its 4096 bytes, and a compressed last page of `n` entries takes at most
/// `12 + 8n` bytes, 4 more than a regular one.
pub(super) fn bound(functions: u64, fdes: u64, compact: u64) -> u64 {
    let compact = compact.div_ceil(COMPACT_ENTRY);
    let entries = 2 * (functions + fdes + compact);
    let pages = entries.div_ceil(PAGE_ENTRIES as u64);
    HEADER
        + 4 * COMMON_MAX as u64
        + 4 * PERSONALITY_MAX as u64
        + INDEX_ENTRY * (pages + 1)
        + LSDA_ENTRY * compact
        + PAGE_HEADER * pages
        + PAGE_ENTRY * entries
        + (COMPRESSED_HEADER - PAGE_HEADER)
}

/// What the last `__unwind_info` was made of, for an image linked again in place.
#[derive(Default)]
pub(super) struct Last {
    /// The [`Placed::synthesis`] the table is of: 0, which no synthesis is, when there is none.
    synthesis: u64,
    base: u64,
    dwarf_mode: u32,
    /// The table, as the last link's state holds it too: a patch copies only the pages it
    /// changes.
    bytes: CowBytes,
    /// A second-level page as a patch writes it again, reused.
    page: Vec<u8>,
    /// Each function's address and size in [`Placed::functions`]' order, which was address
    /// order when `sorted`.
    functions: Vec<(u64, u64)>,
    sorted: bool,
    /// `__eh_frame`'s records in order, its CIEs' FDE pointer encodings by offset, where its
    /// scan stopped, and whether two FDEs started at one address.
    records: Vec<Record>,
    cies: Vec<(usize, u8)>,
    eh_end: usize,
    dups: bool,
    /// The merge's columns, as [`full`] sorts them.
    fdes: Vec<Fde>,
    covered: Vec<Covered>,
    ends: Vec<u64>,
    /// The `__compact_unwind` regions, and each of their entries' function (0: dead) and end
    /// in input order; whether [`Placed::binds`] was in address order.
    compact: Vec<CompactRegion>,
    entries: Vec<(u64, u64)>,
    binds_sorted: bool,
    personalities: Vec<(Held, u32)>,
    rows: Vec<Row>,
    lsdas: Vec<(u32, u32)>,
    /// The address the sentinel is the offset of.
    end: u64,
    encodings: Encodings,
    common: Vec<(u32, u32)>,
    pages: Vec<Page>,
    index_at: u64,
    lsda_at: u64,
    /// A patch's buffers, kept for the next: the addresses whose entries changed, a window's
    /// rows and LSDAs, the rows rewritten, the common encodings counted again.
    changed: Vec<u64>,
    window: Vec<Row>,
    window_lsdas: Vec<(u32, u32)>,
    dirty: Vec<usize>,
    recount: Vec<(u32, u32)>,
}

/// A `__compact_unwind` region: its index in [`Placed::regions`], its range, its first
/// entry's index in [`Last::entries`], and the range of [`Placed::binds`] in it.
#[derive(Clone, Copy)]
struct CompactRegion {
    index: usize,
    addr: u64,
    size: u64,
    first: usize,
    binds: (usize, usize),
}

/// A record of `__eh_frame`: its offset and, of an FDE, the range it covers (0..0 for a dead
/// one); a CIE's `start` is past its `end`.
#[derive(Clone, Copy, Default)]
struct Record {
    at: u64,
    start: u64,
    end: u64,
}

impl Record {
    fn cie(&self) -> bool {
        self.start > self.end
    }
}

/// `__unwind_info` for `placed`, whose `__TEXT` starts at `placed.plan.base`.
pub(super) fn build(placed: &Placed<'_>, arch: Arch) -> Result<Vec<u8>, LinkError> {
    full(placed, arch, None)
}

/// `build`, patching `last`'s table when `placed.delta` allows it; `last` then holds this one.
pub(super) fn build_kept(
    placed: &Placed<'_>,
    arch: Arch,
    last: &mut Last,
) -> Result<CowBytes, LinkError> {
    kept(placed, arch, last).map(|(bytes, _)| bytes)
}

/// [`build_kept`]'s table, and whether it was patched rather than built whole.
fn kept(placed: &Placed<'_>, arch: Arch, last: &mut Last) -> Result<(CowBytes, bool), LinkError> {
    let from = std::mem::take(&mut last.synthesis);
    if let Some(delta) = placed.delta
        && delta.patches(from, &[delta.functions])
        && patch(placed, arch, delta, last).is_some()
    {
        last.synthesis = placed.synthesis;
        // clone: two reference counts, the table shared with the state that holds it
        return Ok((last.bytes.clone(), true));
    }
    // alloc: once per full link, the table's `Arc`
    let bytes = CowBytes::from_vec(full(placed, arch, Some(&mut *last))?);
    // clone: two reference counts, the table shared with the state that holds it
    last.bytes = bytes.clone();
    last.synthesis = placed.synthesis;
    Ok((bytes, false))
}

fn dwarf_mode(arch: Arch) -> Result<u32, LinkError> {
    match arch {
        Arch::Aarch64 => Ok(ARM64_MODE_DWARF),
        Arch::X86_64 => Ok(X86_64_MODE_DWARF),
        // alloc: error path
        Arch::Wasm32 => Err(error("__unwind_info: wasm32 has no compact unwind encoding".into())),
    }
}

/// The whole table, and into `keep` what [`patch`] needs of it but the table itself.
fn full(
    placed: &Placed<'_>,
    arch: Arch,
    mut keep: Option<&mut Last>,
) -> Result<Vec<u8>, LinkError> {
    let dwarf_mode = dwarf_mode(arch)?;
    let base = placed.plan.base;
    let fdes = fdes(placed, keep.as_deref_mut())?;
    let Compact { mut covered, mut ends, personalities, bytes: compact_bytes } =
        compact(placed, dwarf_mode, &fdes, keep.as_deref_mut())?;
    par::extend(&mut ends, &fdes, |it| it.end);
    if !covered.is_sorted_by_key(|it| it.addr) {
        radix::sort_copy_by_key(&mut covered, |it| it.addr);
    }
    if !par::all(par::windows(&ends, 2), |it| it[0] <= it[1]) {
        radix::sort(&mut ends);
    }
    let Rows { rows, lsdas, last: end, sorted } = rows(placed, dwarf_mode, &covered, &fdes, &ends)?;
    let sentinel = offset(base, end, "the end of the last function")?;
    if rows.len() as u64 > u64::from(u32::MAX) / COMPRESSED_ENTRY {
        // alloc: error path
        return Err(error(format!(
            "__unwind_info: {} entries, past the 32-bit offsets the format has",
            rows.len()
        )));
    }

    let mut encodings = Encodings::new(rows.len());
    // A DWARF-mode row is never counted, and most of a Rust image's are: the others are found
    // in parallel and counted in order.
    // alloc: once per link, one list per piece of rows, empty (no buffer) where all are DWARF mode's
    let counted: Vec<Vec<u32>> = par::map(par::chunks(&rows, CHUNK), |chunk| {
        chunk.iter().filter(|it| it.encoding & MODE_MASK != dwarf_mode).map(|it| it.encoding).collect() // alloc: per piece, none where all are DWARF mode's
    }); // alloc: once per link, one list per piece
    for &encoding in counted.iter().flatten() {
        encodings.count(encoding);
    }
    let mut common = Vec::with_capacity(encodings.len);
    encodings.common(&mut common);
    let mut pages = encodings.pages(&rows, common.len(), dwarf_mode);

    let common_at = HEADER;
    let personalities_at = common_at + 4 * common.len() as u64;
    let index_at = personalities_at + 4 * personalities.len() as u64;
    let lsda_at = index_at + INDEX_ENTRY * (pages.len() as u64 + 1);
    let pages_at = lsda_at + LSDA_ENTRY * lsdas.len() as u64;
    let mut len = pages_at;
    for page in &mut pages {
        page.at = len;
        len += page.len();
    }
    let limit = bound(placed.functions.len() as u64, fdes.len() as u64, compact_bytes);
    if len > limit {
        // alloc: error path
        return Err(error(format!(
            "__unwind_info: {} entries take {len} bytes, past the {limit} sized for {} functions, \
             {} FDEs and {} compact unwind entries",
            rows.len(),
            placed.functions.len(),
            fdes.len(),
            compact_bytes / COMPACT_ENTRY
        )));
    }
    if len > u64::from(u32::MAX) {
        // alloc: error path
        return Err(error(format!(
            "__unwind_info: {len} bytes, past the 32-bit offsets the format has"
        )));
    }
    let small = |value: u64| value as u32;

    // alloc: the table itself, which the writer returns: once per link, at its exact size
    let mut bytes = vec![0; len as usize];
    let mut out = Out { bytes: &mut bytes, at: 0 };
    for value in [
        VERSION,
        small(common_at),
        common.len() as u32,
        small(personalities_at),
        personalities.len() as u32,
        small(index_at),
        pages.len() as u32 + 1,
    ] {
        out.u32(value);
    }
    for &(_, encoding) in &common {
        out.u32(encoding);
    }
    for &(_, cell) in &personalities {
        out.u32(cell);
    }
    let mut lsda_index = 0usize;
    for page in &pages {
        let first = rows[page.first].function;
        while lsdas.get(lsda_index).is_some_and(|it| it.0 < first) {
            lsda_index += 1;
        }
        out.u32(first);
        out.u32(small(page.at));
        out.u32(small(lsda_at + LSDA_ENTRY * lsda_index as u64));
    }
    out.u32(sentinel);
    out.u32(0);
    out.u32(small(lsda_at + LSDA_ENTRY * lsdas.len() as u64));
    for &(function, lsda) in &lsdas {
        out.u32(function);
        out.u32(lsda);
    }
    debug_assert_eq!(out.at as u64, pages_at);
    let mut rest = &mut bytes[pages_at as usize..];
    // alloc: once per link, one slice per page
    let mut slices: Vec<(&Page, &mut [u8])> = Vec::with_capacity(pages.len());
    for page in &pages {
        let (these, after) = std::mem::take(&mut rest).split_at_mut(page.len() as usize);
        slices.push((page, these));
        rest = after;
    }
    debug_assert!(rest.is_empty());
    // A compressed page stamps the encodings' indexes of its own: those one after another, the
    // regular pages in parallel.
    for slot in slices.iter_mut().filter(|it| it.0.compressed) {
        let page = slot.0;
        write_page(&mut Out { bytes: &mut *slot.1, at: 0 }, &rows, page, common.len(), &mut encodings, dwarf_mode);
    }
    par::for_each(slices, |(page, out)| {
        if !page.compressed {
            write_regular(&mut Out { bytes: out, at: 0 }, &rows, page);
        }
    });
    if let Some(last) = keep {
        last.base = base;
        last.dwarf_mode = dwarf_mode;
        last.functions.clear();
        par::extend(&mut last.functions, placed.functions, |it| (it.addr, it.size));
        last.sorted = sorted;
        last.fdes = fdes;
        last.covered = covered;
        last.ends = ends;
        last.personalities = personalities;
        last.rows = rows;
        last.lsdas = lsdas;
        last.end = end;
        last.encodings = encodings;
        last.common = common;
        last.pages = pages;
        last.index_at = index_at;
        last.lsda_at = lsda_at;
    }
    Ok(bytes)
}

/// Writes `page` of `rows` at `out.at`, and moves `out.at` past it.
fn write_page(
    out: &mut Out<'_>,
    rows: &[Row],
    page: &Page,
    common: usize,
    encodings: &mut Encodings,
    dwarf_mode: u32,
) {
    if !page.compressed {
        write_regular(out, rows, page);
        return;
    }
    let rows = &rows[page.first..page.first + page.count];
    let entries = COMPRESSED_ENTRY as usize * rows.len();
    out.u32(SECOND_LEVEL_COMPRESSED);
    out.u16(COMPRESSED_HEADER as u16);
    out.u16(rows.len() as u16);
    out.u16((COMPRESSED_HEADER as usize + entries) as u16);
    out.u16(page.locals as u16);
    let locals_at = out.at + entries;
    let first = rows[0].function;
    let mut next = common;
    encodings.stamp += 1;
    for row in rows {
        let dwarf = row.encoding & MODE_MASK == dwarf_mode;
        let (index, new) = encodings.index(row.encoding, dwarf, next);
        if new {
            out.put(locals_at + 4 * (next - common), row.encoding);
            next += 1;
        }
        out.u32((row.function - first) | (u32::from(index) << 24));
    }
    debug_assert_eq!(next - common, page.locals);
    out.at = locals_at + 4 * page.locals;
}

/// Writes the regular `page` of `rows` at `out.at`, and moves `out.at` past it.
fn write_regular(out: &mut Out<'_>, rows: &[Row], page: &Page) {
    let rows = &rows[page.first..page.first + page.count];
    out.u32(SECOND_LEVEL_REGULAR);
    out.u16(PAGE_HEADER as u16);
    out.u16(rows.len() as u16);
    for row in rows {
        out.u32(row.function);
        out.u32(row.encoding);
    }
}

/// Patches `last`'s table into `placed`'s, which `delta` says how it differs from; `None` where
/// a patch would not be exact, and `last` is to be built again. Exact when every row the
/// changes touch keeps its place: as many rows from each changed address to the first
/// unchanged entry after it, each with an LSDA where it had one, every page split as before
/// over the same common encodings and personalities, and each changed FDE and compact entry
/// alone at its address and still in order in its column.
fn patch(placed: &Placed<'_>, arch: Arch, delta: &Delta<'_>, last: &mut Last) -> Option<()> {
    let dwarf_mode = dwarf_mode(arch).ok()?;
    let functions = placed.functions;
    if dwarf_mode != last.dwarf_mode
        || placed.plan.base != last.base
        || !last.sorted
        || !last.binds_sorted
        || functions.len() != last.functions.len()
    {
        return None;
    }
    last.changed.clear();
    // The greatest end the changes removed and the greatest they made: the sentinel's.
    let (mut gone, mut came) = (0u64, 0u64);
    for splice in delta.functions {
        let (from, to) = (splice.new.start as usize, splice.new.end as usize);
        let old = last.functions.get_mut(splice.old.start as usize..splice.old.end as usize)?;
        let new = functions.get(from..to)?;
        for (old, new) in old.iter_mut().zip(new) {
            if old.0 != new.addr {
                last.changed.push(old.0);
                last.changed.push(new.addr);
            }
            gone = gone.max(old.0.saturating_add(old.1));
            came = came.max(new.addr.saturating_add(new.size));
            *old = (new.addr, new.size);
        }
        let edges = &functions[from.saturating_sub(1)..(to + 1).min(functions.len())];
        if !edges.is_sorted_by_key(|it| it.addr) {
            return None;
        }
    }

    // A personality cell's bind is part of its entry: none may come, go or move.
    let binds = placed.binds;
    for splice in delta.binds {
        let (old_from, old_to) = (splice.old.start as usize, splice.old.end as usize);
        if last.compact.iter().any(|it| old_from < it.binds.1 && it.binds.0 < old_to) {
            return None;
        }
        let (from, to) = (splice.new.start as usize, splice.new.end as usize);
        let new = binds.get(from..to)?;
        let in_compact = |addr: u64| {
            last.compact.iter().any(|it| addr >= it.addr && addr - it.addr < it.size)
        };
        if new.iter().any(|it| in_compact(it.addr)) {
            return None;
        }
        let edges = &binds[from.saturating_sub(1)..(to + 1).min(binds.len())];
        if !edges.is_sorted_by_key(|it| it.addr) {
            return None;
        }
    }
    if !delta.binds.is_empty() {
        for region in &mut last.compact {
            let from = binds.partition_point(|it| it.addr < region.addr);
            let to = binds.partition_point(|it| it.addr < region.addr.saturating_add(region.size));
            if to - from != region.binds.1 - region.binds.0 {
                return None;
            }
            region.binds = (from, to);
        }
    }

    if let Some((index, region)) = placed.region(&Class::EhFrame) {
        let bytes = region_bytes(placed, index, region, "__eh_frame").ok()?;
        // alloc: only for a record across two pages, grown to the longest such record once per patch
        let mut across = Vec::new();
        for (start, stop) in overlaps(delta.written, region.addr, region.size) {
            if stop > last.eh_end as u64 {
                return None;
            }
            let mut at = last.records.partition_point(|it| it.at <= start).checked_sub(1)?;
            while last.records.get(at).is_some_and(|it| it.at < stop) {
                last.refresh_fde(bytes, region.addr, at, &mut across, &mut gone, &mut came)?;
                at += 1;
            }
        }
    }
    for k in 0..last.compact.len() {
        let region = last.compact[k];
        let placed_region = placed
            .regions
            .get(region.index)
            .filter(|it| it.addr == region.addr && it.size == region.size)?;
        let bytes = region_bytes(placed, region.index, placed_region, "__compact_unwind").ok()?;
        for (start, stop) in overlaps(delta.written, region.addr, region.size) {
            let to = stop.div_ceil(COMPACT_ENTRY).min(region.size / COMPACT_ENTRY);
            for n in start / COMPACT_ENTRY..to {
                let n = n as usize;
                last.refresh_entry(placed, bytes, region, n, dwarf_mode, &mut gone, &mut came)?;
            }
        }
    }
    let end = if gone < last.end {
        last.end.max(came)
    } else if came >= last.end {
        came
    } else {
        return None;
    };

    last.changed.sort_unstable();
    last.changed.dedup();
    last.dirty.clear();
    let mut recount = false;
    let mut i = 0;
    while i < last.changed.len() {
        let mut j = i;
        let hi = loop {
            match last.after(functions, last.changed[j]) {
                Some(next) if last.changed.get(j + 1).is_some_and(|it| *it <= next) => j += 1,
                Some(next) => break Some(next),
                None => {
                    j = last.changed.len() - 1;
                    break None;
                }
            }
        };
        let lo = last.changed[i];
        recount |= last.window(placed, dwarf_mode, lo, hi)?;
        i = j + 1;
    }

    let common = last.common.len();
    if recount {
        last.encodings.common(&mut last.recount);
        let same = last.recount.len() == common
            && last.recount.iter().zip(&last.common).all(|(new, old)| new.1 == old.1);
        if !same {
            return None;
        }
        std::mem::swap(&mut last.common, &mut last.recount);
    }
    // Each page a rewritten row is in splits as before, and so does the one before it, whose
    // end its first row may decide.
    let mut previous = usize::MAX;
    for k in 0..last.dirty.len() {
        let row = last.dirty[k];
        let page = last.pages.partition_point(|it| it.first <= row).checked_sub(1)?;
        if page == previous {
            continue;
        }
        previous = page;
        for page in [page.checked_sub(1), Some(page)].into_iter().flatten() {
            let kept = last.pages[page];
            let split = last.encodings.page(&last.rows, kept.first, common, dwarf_mode);
            let same = (split.count, split.compressed, split.locals)
                == (kept.count, kept.compressed, kept.locals);
            if !same {
                return None;
            }
        }
    }
    let mut previous = usize::MAX;
    for k in 0..last.dirty.len() {
        let row = last.dirty[k];
        let page = last.pages.partition_point(|it| it.first <= row).checked_sub(1)?;
        if page == previous {
            continue;
        }
        previous = page;
        let kept = last.pages[page];
        last.page.clear();
        last.page.resize(kept.len() as usize, 0);
        let mut out = Out { bytes: &mut last.page, at: 0 };
        write_page(&mut out, &last.rows, &kept, common, &mut last.encodings, dwarf_mode);
        let written = out.at;
        debug_assert_eq!(written, last.page.len());
        last.bytes.update(kept.at as usize, &last.page);
        let at = last.index_at + INDEX_ENTRY * page as u64;
        last.bytes.update(at as usize, &last.rows[kept.first].function.to_le_bytes());
    }
    if end != last.end {
        let sentinel = offset(last.base, end, "the end of the last function").ok()?;
        let at = last.index_at + INDEX_ENTRY * last.pages.len() as u64;
        last.bytes.update(at as usize, &sentinel.to_le_bytes());
        last.end = end;
    }
    Some(())
}

impl Last {
    /// Reads again the record of `__eh_frame` (`bytes`, at `section`) at `records[at]`, which
    /// must keep its length and its kind, and of a CIE its FDE pointer encoding (all of it an
    /// FDE reads); moves an FDE in the columns. `across` holds a record across two pages.
    fn refresh_fde(
        &mut self,
        bytes: View<'_>,
        section: u64,
        at: usize,
        across: &mut Vec<u8>,
        gone: &mut u64,
        came: &mut u64,
    ) -> Option<()> {
        let old = self.records[at];
        let next = self.records.get(at + 1).map_or(self.eh_end as u64, |it| it.at);
        let (parsed, stop) = record(bytes, old.at as usize, section, &self.cies, across).ok()?;
        if stop as u64 != next {
            return None;
        }
        let (start, end) = match parsed {
            Parsed::Cie(encoding) if old.cie() => {
                let cie = self.cies.binary_search_by_key(&(old.at as usize), |it| it.0).ok()?;
                return (self.cies[cie].1 == encoding).then_some(());
            }
            Parsed::Fde { start, end } if !old.cie() => (start, end),
            _ => return None,
        };
        if (start, end) == (old.start, old.end) {
            return Some(());
        }
        if self.dups || start == 0 || old.start == 0 {
            return None;
        }
        // A DWARF-mode compact entry names the FDE at its address.
        for addr in [old.start, start] {
            let covered = span(&self.covered, addr, Some(addr), |it| it.addr);
            if covered.iter().any(|it| matches!(it.kind, Kind::Dwarf { .. })) {
                return None;
            }
        }
        let pos = self.fdes.binary_search_by_key(&old.start, |it| it.addr).ok()?;
        let ordered = (pos == 0 || self.fdes[pos - 1].addr < start)
            && self.fdes.get(pos + 1).is_none_or(|it| start < it.addr);
        if self.fdes[pos].offset != old.at || !ordered {
            return None;
        }
        self.fdes[pos] = Fde { addr: start, end, offset: old.at };
        replace(&mut self.ends, old.end, end)?;
        self.records[at] = Record { at: old.at, start, end };
        self.changed.extend_from_slice(&[old.start, old.end, start, end]);
        *gone = (*gone).max(old.end);
        *came = (*came).max(end);
        Some(())
    }

    /// Reads again entry `n` of the `__compact_unwind` region `region` (`bytes`), which must
    /// stay alive or dead, keep its personality, and stay alone at its address in its column.
    #[allow(clippy::too_many_arguments)]
    fn refresh_entry(
        &mut self,
        placed: &Placed<'_>,
        bytes: View<'_>,
        region: CompactRegion,
        n: usize,
        dwarf_mode: u32,
        gone: &mut u64,
        came: &mut u64,
    ) -> Option<()> {
        let size = COMPACT_ENTRY as usize;
        let entry: [u8; COMPACT_ENTRY as usize] = bytes.array(n * size)?;
        let entry = &entry[..];
        let index = region.first + n;
        let (old_function, old_end) = *self.entries.get(index)?;
        let function = u64_at(entry, 0);
        if (function == 0) != (old_function == 0) {
            return None;
        }
        if function == 0 {
            return Some(());
        }
        let end = function.checked_add(u64::from(u32_at(entry, 8)))?;
        let encoding = u32_at(entry, 12);
        let (kind, personality) = if encoding & MODE_MASK == dwarf_mode {
            let at = self.fdes.partition_point(|it| it.addr < function);
            let fde = self.fdes.get(at).filter(|it| it.addr == function)?;
            (Kind::Dwarf { fde: fde.offset }, 0)
        } else {
            let cell = region.addr + COMPACT_ENTRY * n as u64 + 16;
            let personality = match held(placed.binds, cell, u64_at(entry, 16)) {
                Some(held) => self.personalities.iter().position(|(it, _)| *it == held)? as u32 + 1,
                None => 0,
            };
            let lsda = u64_at(entry, 24);
            let mut encoding =
                (encoding & !(PERSONALITY_MASK | HAS_LSDA)) | (personality << PERSONALITY_SHIFT);
            if lsda != 0 {
                encoding |= HAS_LSDA;
            }
            (Kind::Compact { encoding, lsda }, personality)
        };
        let pos = self.covered.binary_search_by_key(&old_function, |it| it.addr).ok()?;
        let (low, high) = (old_function.min(function), old_function.max(function));
        let alone = (pos == 0 || self.covered[pos - 1].addr < low)
            && self.covered.get(pos + 1).is_none_or(|it| high < it.addr);
        if !alone {
            return None;
        }
        let old = self.covered[pos].kind;
        let old_personality = match old {
            Kind::Compact { encoding, .. } => (encoding & PERSONALITY_MASK) >> PERSONALITY_SHIFT,
            Kind::Dwarf { .. } => 0,
        };
        if personality != old_personality {
            return None;
        }
        if (function, end, kind) == (old_function, old_end, old) {
            return Some(());
        }
        self.covered[pos] = Covered { addr: function, kind };
        replace(&mut self.ends, old_end, end)?;
        self.entries[index] = (function, end);
        self.changed.extend_from_slice(&[old_function, old_end, function, end]);
        *gone = (*gone).max(old_end);
        *came = (*came).max(end);
        Some(())
    }

    /// The first address past `addr` a function or a column's entry is at.
    fn after(&self, functions: &[Function], addr: u64) -> Option<u64> {
        let starts = functions.partition_point(|it| it.addr <= addr);
        let covered = self.covered.partition_point(|it| it.addr <= addr);
        let fdes = self.fdes.partition_point(|it| it.addr <= addr);
        let ends = self.ends.partition_point(|it| *it <= addr);
        [
            functions.get(starts).map(|it| it.addr),
            self.covered.get(covered).map(|it| it.addr),
            self.fdes.get(fdes).map(|it| it.addr),
            self.ends.get(ends).copied(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Merges the entries from `lo` to `hi` (to the end: `None`) again over the rows there,
    /// which they must match in number and in which have an LSDA; whether an encoding changed.
    fn window(
        &mut self,
        placed: &Placed<'_>,
        dwarf_mode: u32,
        lo: u64,
        hi: Option<u64>,
    ) -> Option<bool> {
        let lo_offset = u32::try_from(lo.checked_sub(self.base)?).ok()?;
        let hi_offset = match hi {
            Some(hi) => u32::try_from(hi.checked_sub(self.base)?).ok()?,
            None => u32::MAX,
        };
        let columns = Columns {
            covered: span(&self.covered, lo, hi, |it| it.addr),
            fdes: span(&self.fdes, lo, hi, |it| it.addr),
            starts: Starts::Placed(span(placed.functions, lo, hi, |it| it.addr)),
            ends: span(&self.ends, lo, hi, |it| *it),
        };
        let from = self.rows.partition_point(|it| it.function < lo_offset);
        let to = self.rows.partition_point(|it| it.function <= hi_offset);
        let before = from.checked_sub(1).map(|at| self.rows[at]);
        self.window.clear();
        self.window_lsdas.clear();
        merge(placed.plan.base, dwarf_mode, columns, before, &mut self.window, &mut self.window_lsdas).ok()?;
        if self.window.len() != to - from {
            return None;
        }
        let lsdas_from = self.lsdas.partition_point(|it| it.0 < lo_offset);
        let mut recount = false;
        for (k, &new) in self.window.iter().enumerate() {
            let old = self.rows[from + k];
            if (old.encoding ^ new.encoding) & HAS_LSDA != 0 {
                return None;
            }
            if old == new {
                continue;
            }
            if old.encoding != new.encoding {
                if old.encoding & MODE_MASK != dwarf_mode {
                    self.encodings.uncount(old.encoding);
                }
                if new.encoding & MODE_MASK != dwarf_mode {
                    self.encodings.count(new.encoding);
                }
                recount = true;
            }
            self.rows[from + k] = new;
            self.dirty.push(from + k);
        }
        let lsdas = self.lsdas.get_mut(lsdas_from..lsdas_from + self.window_lsdas.len())?;
        for (k, (old, &new)) in lsdas.iter_mut().zip(&self.window_lsdas).enumerate() {
            if *old != new {
                *old = new;
                let at = (self.lsda_at + LSDA_ENTRY * (lsdas_from + k) as u64) as usize;
                self.bytes.update(at, &new.0.to_le_bytes());
                self.bytes.update(at + 4, &new.1.to_le_bytes());
            }
        }
        Some(recount)
    }
}

/// The part of the sorted `items` whose `key` is from `lo` to `hi` (to the end: `None`).
fn span<T>(items: &[T], lo: u64, hi: Option<u64>, key: impl Fn(&T) -> u64) -> &[T] {
    let from = items.partition_point(|it| key(it) < lo);
    let to = match hi {
        Some(hi) => items.partition_point(|it| key(it) <= hi),
        None => items.len(),
    };
    &items[from..to.max(from)]
}

/// The parts of `written` (by address, none overlapping) within `size` bytes from `addr`, as
/// offsets from it.
fn overlaps(written: &[(u64, u64)], addr: u64, size: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
    let end = addr.saturating_add(size);
    let from = written.partition_point(|it| it.0.saturating_add(it.1) <= addr);
    written[from..]
        .iter()
        .take_while(move |it| it.0 < end)
        .map(move |it| (it.0.max(addr) - addr, it.0.saturating_add(it.1).min(end) - addr))
}

/// Replaces one `old` of the sorted `values` with `new`, where that keeps them sorted.
fn replace(values: &mut [u64], old: u64, new: u64) -> Option<()> {
    if old == new {
        return Some(());
    }
    let from = values.partition_point(|it| *it < old);
    let to = values.partition_point(|it| *it <= old);
    if from == to {
        return None;
    }
    let (at, sorted) = if new > old {
        (to - 1, values.get(to).is_none_or(|it| new <= *it))
    } else {
        (from, from == 0 || values[from - 1] <= new)
    };
    if !sorted {
        return None;
    }
    values[at] = new;
    Some(())
}

/// What holds the personality of the compact unwind entry whose personality cell is `cell`:
/// the bind there, else the address the cell holds (0: none).
fn held(binds: &[Bind], cell: u64, personality: u64) -> Option<Held> {
    let upto = binds.partition_point(|it| it.addr <= cell);
    match upto.checked_sub(1).map(|at| binds[at]).filter(|it| it.addr == cell) {
        Some(bind) => Some(Held::Import { import: bind.import, addend: bind.addend }),
        None => (personality != 0).then_some(Held::Address(personality)),
    }
}

/// What a personality cell holds: the loader slides an address of the image, or binds an
/// import.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    Address(u64),
    Import { import: u32, addend: i64 },
}

/// A start a `__compact_unwind` entry covers: 24 bytes, a column sorted by `addr`.
#[derive(Clone, Copy)]
struct Covered {
    addr: u64,
    kind: Kind,
}

const _: () = assert!(size_of::<Covered>() == 24);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The encoding with its personality index and `UNWIND_HAS_LSDA`, and the LSDA's address
    /// (0: none).
    Compact { encoding: u32, lsda: u64 },
    /// The FDE's offset in `__eh_frame`.
    Dwarf { fde: u64 },
}

/// An FDE of `__eh_frame`: 24 bytes, a column sorted by `addr`, one per address.
#[derive(Clone, Copy, Debug, Default)]
struct Fde {
    addr: u64,
    end: u64,
    offset: u64,
}

const _: () = assert!(size_of::<Fde>() == 24);

/// A second-level entry, offsets from the image's base: 8 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Row {
    function: u32,
    encoding: u32,
}

const _: () = assert!(size_of::<Row>() == 8);

/// What the `__compact_unwind` regions give: their starts in input order (sorted by the
/// caller), the ends of their ranges, the personalities in order of first use and their
/// cells' offsets, and how many bytes they had.
struct Compact {
    covered: Vec<Covered>,
    ends: Vec<u64>,
    personalities: Vec<(Held, u32)>,
    bytes: u64,
}

fn compact(
    placed: &Placed<'_>,
    dwarf_mode: u32,
    fdes: &[Fde],
    mut keep: Option<&mut Last>,
) -> Result<Compact, LinkError> {
    let base = placed.plan.base;
    let binds: Cow<'_, [Bind]> = if placed.binds.is_sorted_by_key(|it| it.addr) {
        Cow::Borrowed(placed.binds)
    } else {
        // alloc: only for binds out of order, which `Placed` lists in order; once per link
        let mut binds = placed.binds.to_vec();
        radix::sort_copy_by_key(&mut binds, |it| it.addr);
        Cow::Owned(binds)
    };
    if let Some(last) = keep.as_deref_mut() {
        last.compact.clear();
        last.entries.clear();
        last.binds_sorted = matches!(binds, Cow::Borrowed(_));
    }
    let class = super::compact_unwind();
    // Every entry's start and end, and the FDEs' ends: each column allocated once.
    let mut entries = 0;
    for region in placed.regions.iter().filter(|it| it.class == class) {
        entries += (region.size / COMPACT_ENTRY) as usize;
    }
    let mut covered = Vec::with_capacity(entries);
    let mut ends = Vec::with_capacity(entries + fdes.len());
    let mut personalities: Vec<(Held, u32)> = Vec::with_capacity(PERSONALITY_MAX);
    let mut compact_bytes = 0u64;
    for (index, region) in placed.regions.iter().enumerate().filter(|(_, it)| it.class == class) {
        let bytes = region_bytes(placed, index, region, "__compact_unwind")?;
        if bytes.len() as u64 % COMPACT_ENTRY != 0 {
            // alloc: error path
            return Err(error(format!(
                "__compact_unwind at {:#x}: {} bytes, not a whole number of 32-byte entries",
                region.addr,
                bytes.len()
            )));
        }
        compact_bytes += bytes.len() as u64;
        if let Some(last) = keep.as_deref_mut() {
            let end = region.addr.saturating_add(region.size);
            let from = binds.partition_point(|it| it.addr < region.addr);
            let to = binds.partition_point(|it| it.addr < end);
            let first = last.entries.len();
            let (addr, size) = (region.addr, region.size);
            last.compact.push(CompactRegion { index, addr, size, first, binds: (from, to) });
        }
        for (n, entry) in bytes.arrays::<{ COMPACT_ENTRY as usize }>(0, bytes.len()).enumerate() {
            let entry = &entry[..];
            let function = u64_at(entry, 0);
            if function == 0 {
                if let Some(last) = keep.as_deref_mut() {
                    last.entries.push((0, 0));
                }
                continue;
            }
            let length = u64::from(u32_at(entry, 8));
            let encoding = u32_at(entry, 12);
            let personality = u64_at(entry, 16);
            let lsda = u64_at(entry, 24);
            let end = function.checked_add(length).ok_or_else(|| {
                // alloc: error path
                error(format!(
                    "__compact_unwind: {} has length {length:#x}, past the end of the address \
                     space",
                    named(placed, function)
                ))
            })?;
            let kind = if encoding & MODE_MASK == dwarf_mode {
                let at = fdes.partition_point(|it| it.addr < function);
                let Some(fde) = fdes.get(at).filter(|it| it.addr == function) else {
                    // alloc: error path
                    return Err(error(format!(
                        "__compact_unwind: the entry of {} says DWARF mode ({encoding:#010x}), \
                         but no FDE in __eh_frame starts there",
                        named(placed, function)
                    )));
                };
                Kind::Dwarf { fde: fde.offset }
            } else {
                let mut encoding = encoding & !(PERSONALITY_MASK | HAS_LSDA);
                let cell = region.addr + COMPACT_ENTRY * n as u64 + 16;
                if let Some(held) = held(&binds, cell, personality) {
                    let index = match personalities.iter().position(|(it, _)| *it == held) {
                        Some(index) => index,
                        None => {
                            if personalities.len() == PERSONALITY_MAX {
                                // alloc: error path
                                return Err(error(format!(
                                    "__unwind_info: {} has a fourth distinct personality \
                                     (held at {cell:#x}); the format has room for \
                                     {PERSONALITY_MAX}",
                                    named(placed, function)
                                )));
                            }
                            let cell = offset(base, cell, "a personality's cell")?;
                            personalities.push((held, cell));
                            personalities.len() - 1
                        }
                    };
                    encoding |= ((index as u32) + 1) << PERSONALITY_SHIFT;
                }
                if lsda != 0 {
                    encoding |= HAS_LSDA;
                }
                Kind::Compact { encoding, lsda }
            };
            covered.push(Covered { addr: function, kind });
            ends.push(end);
            if let Some(last) = keep.as_deref_mut() {
                last.entries.push((function, end));
            }
        }
    }
    Ok(Compact { covered, ends, personalities, bytes: compact_bytes })
}

/// Function starts in address order: [`Placed::functions`] as the core gives them, or their
/// addresses sorted here when they are not.
#[derive(Clone, Copy)]
enum Starts<'a> {
    Placed(&'a [Function]),
    Sorted(&'a [u64]),
}

impl<'a> Starts<'a> {
    fn get(&self, index: usize) -> Option<u64> {
        match self {
            Starts::Placed(functions) => functions.get(index).map(|it| it.addr),
            Starts::Sorted(starts) => starts.get(index).copied(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Starts::Placed(functions) => functions.len(),
            Starts::Sorted(starts) => starts.len(),
        }
    }

    fn between(self, lo: Option<u64>, hi: Option<u64>) -> Starts<'a> {
        match self {
            Starts::Placed(functions) => Starts::Placed(between(functions, lo, hi, |it| it.addr)),
            Starts::Sorted(starts) => Starts::Sorted(between(starts, lo, hi, |it| *it)),
        }
    }
}

/// The part of the sorted `items` whose `key` is from `lo` (from the start: `None`) to below
/// `hi` (to the end: `None`).
fn between<T>(items: &[T], lo: Option<u64>, hi: Option<u64>, key: impl Fn(&T) -> u64) -> &[T] {
    let from = lo.map_or(0, |lo| items.partition_point(|it| key(it) < lo));
    let to = hi.map_or(items.len(), |hi| items.partition_point(|it| key(it) < hi));
    &items[from..to.max(from)]
}

/// The second-level entries in address order, the LSDA index (function and LSDA offsets),
/// the end of the last function, and whether [`Placed::functions`] was in address order.
struct Rows {
    rows: Vec<Row>,
    lsdas: Vec<(u32, u32)>,
    last: u64,
    sorted: bool,
}

/// The sorted columns [`merge`] reads.
#[derive(Clone, Copy)]
struct Columns<'c> {
    covered: &'c [Covered],
    fdes: &'c [Fde],
    starts: Starts<'c>,
    ends: &'c [u64],
}

impl<'c> Columns<'c> {
    /// The entries from `lo` to below `hi` ([`between`]).
    fn between(&self, lo: Option<u64>, hi: Option<u64>) -> Columns<'c> {
        Columns {
            covered: between(self.covered, lo, hi, |it| it.addr),
            fdes: between(self.fdes, lo, hi, |it| it.addr),
            starts: self.starts.between(lo, hi),
            ends: between(self.ends, lo, hi, |it| *it),
        }
    }

    fn len(&self) -> usize {
        self.covered.len() + self.fdes.len() + self.starts.len() + self.ends.len()
    }

    /// What [`merge`] needs of the row its walk of these columns leaves last: the encoding of
    /// their greatest address (its compact entry's, its FDE's or 0), which the last row has
    /// whether that address added one or not, as an entry adds none only with the encoding
    /// before and no LSDA. None for no entries.
    fn tail(&self, dwarf_mode: u32) -> Option<Row> {
        let heads = [
            self.covered.last().map(|it| it.addr),
            self.fdes.last().map(|it| it.addr),
            self.starts.get(self.starts.len().wrapping_sub(1)),
            self.ends.last().copied(),
        ];
        let at = heads.into_iter().flatten().max()?;
        let compact = self.covered.partition_point(|it| it.addr < at);
        let encoding = match self.covered.get(compact).filter(|it| it.addr == at) {
            Some(Covered { kind: Kind::Compact { encoding, .. }, .. }) => *encoding,
            Some(Covered { kind: Kind::Dwarf { fde }, .. }) => dwarf_encoding(dwarf_mode, *fde),
            None => match self.fdes.binary_search_by_key(&at, |it| it.addr) {
                Ok(found) => dwarf_encoding(dwarf_mode, self.fdes[found].offset),
                Err(_) => 0,
            },
        };
        Some(Row { function: 0, encoding })
    }
}

/// The DWARF-mode encoding naming the FDE at `fde` in `__eh_frame`. An FDE past the field's
/// 24 bits gets offset 0: libunwind takes the offset only as where to start looking, and with
/// 0 scans `__eh_frame` from its start and caches the FDE it finds
/// (`UnwindCursor::getInfoFromDwarfSection`), so a program of more than 16 MiB of frames
/// unwinds.
fn dwarf_encoding(dwarf_mode: u32, fde: u64) -> u32 {
    match u32::try_from(fde) {
        Ok(offset) if offset <= DWARF_OFFSET_MASK => dwarf_mode | offset,
        _ => dwarf_mode,
    }
}

/// Below two of this many function starts the merge is one; a piece covers at least a quarter
/// of it.
const MERGE: usize = 16 * 1024;

/// The rows of every entry, by [`merge`]: in pieces cut at function starts, as many as the
/// pool has threads (so every entry of an address is in one piece, and the bytes are the same
/// on any number of threads), merged in parallel, each after the row the
/// piece before leaves last ([`Columns::tail`]), and joined in order: the one merge's rows.
fn rows(
    placed: &Placed<'_>,
    dwarf_mode: u32,
    covered: &[Covered],
    fdes: &[Fde],
    ends: &[u64],
) -> Result<Rows, LinkError> {
    let base = placed.plan.base;
    let functions = placed.functions;
    let last = par::map(functions, |it| it.addr.saturating_add(it.size)).into_iter().fold(base, u64::max);
    let sorted = par::all(par::windows(functions, 2), |it| it[0].addr <= it[1].addr);
    // alloc: only for functions out of address order, which `Placed` lists in order
    let mut held = Vec::new();
    let starts = if sorted {
        Starts::Placed(functions)
    } else {
        held.reserve_exact(functions.len());
        held.extend(functions.iter().map(|it| it.addr));
        radix::sort(&mut held);
        Starts::Sorted(&held)
    };
    let columns = Columns { covered, fdes, starts, ends };
    if starts.len() < 2 * MERGE {
        // alloc: once per link, the rows: at most one an entry of a column
        let mut rows: Vec<Row> = Vec::with_capacity(columns.len());
        // An LSDA is a compact entry's: at most one each.
        let mut lsdas = Vec::with_capacity(covered.len());
        let greatest = merge(base, dwarf_mode, columns, None, &mut rows, &mut lsdas)?;
        return Ok(Rows { rows, lsdas, last: last.max(greatest), sorted });
    }
    // alloc: once per link, one cut per piece
    let every = (MERGE / 4).max(starts.len().div_ceil(par::threads().max(1)));
    let mut cuts: Vec<u64> = Vec::with_capacity(starts.len() / every);
    for k in (every..starts.len()).step_by(every) {
        let at = starts.get(k).expect("a start below the count");
        if cuts.last().is_none_or(|it| *it < at) {
            cuts.push(at);
        }
    }
    let piece = |nth: usize| -> Result<(Vec<Row>, Vec<(u32, u32)>, u64), LinkError> {
        let lo = nth.checked_sub(1).map(|it| cuts[it]);
        let these = columns.between(lo, cuts.get(nth).copied());
        let before = match nth {
            0 => None,
            1 => columns.between(None, lo).tail(dwarf_mode),
            _ => columns.between(Some(cuts[nth - 2]), lo).tail(dwarf_mode),
        };
        // alloc: once per piece, its rows and LSDAs
        let (mut rows, mut lsdas) = (Vec::with_capacity(these.len()), Vec::with_capacity(these.covered.len()));
        let greatest = merge(base, dwarf_mode, these, before, &mut rows, &mut lsdas)?;
        Ok((rows, lsdas, greatest))
    };
    // alloc: once per link, one result per piece
    let pieces: Vec<Result<(Vec<Row>, Vec<(u32, u32)>, u64), LinkError>> =
        par::map(0..cuts.len() + 1, piece); // alloc: once per link, one result per piece
    // alloc: once per link, one entry per piece
    let mut done = Vec::with_capacity(pieces.len());
    for piece in pieces {
        done.push(piece?);
    }
    let greatest = done.iter().map(|it| it.2).max().unwrap_or(0);
    // alloc: once per link, one slice per piece of rows and of LSDAs
    let rows: Vec<&[Row]> = done.iter().map(|it| &it.0[..]).collect(); // alloc: once per link, a slice per piece
    let lsdas: Vec<&[(u32, u32)]> = done.iter().map(|it| &it.1[..]).collect(); // alloc: once per link, a slice per piece
    Ok(Rows { rows: concat(&rows), lsdas: concat(&lsdas), last: last.max(greatest), sorted })
}

/// One merge of the sorted columns into `rows` and `lsdas`, after `before`, the row before
/// them: of one address the compact entry wins, then the FDE, then a function without either
/// (encoding 0), then the end of a covered range (encoding 0); an entry with the encoding of
/// the one before and neither with an LSDA adds nothing (as ld64). The greatest address seen.
/// `base` is the image's.
fn merge(
    base: u64,
    dwarf_mode: u32,
    columns: Columns<'_>,
    before: Option<Row>,
    rows: &mut Vec<Row>,
    lsdas: &mut Vec<(u32, u32)>,
) -> Result<u64, LinkError> {
    let Columns { covered, fdes, starts, ends } = columns;
    // The row before has an LSDA only as the entry just before: the next is never merged.
    let mut previous = before.map(|it| it.encoding);
    let mut lsda_last = before.is_some_and(|it| it.encoding & HAS_LSDA != 0);
    let mut greatest = 0;
    let (mut c, mut f, mut s, mut e) = (0, 0, 0, 0);
    loop {
        let heads = [
            covered.get(c).map(|it| it.addr),
            fdes.get(f).map(|it| it.addr),
            starts.get(s),
            ends.get(e).copied(),
        ];
        let Some(at) = heads.into_iter().flatten().min() else {
            break;
        };
        let kind = if heads[0] == Some(at) {
            Some(covered[c].kind)
        } else if heads[1] == Some(at) {
            Some(Kind::Dwarf { fde: fdes[f].offset })
        } else {
            None
        };
        while covered.get(c).is_some_and(|it| it.addr == at) {
            c += 1;
        }
        if heads[1] == Some(at) {
            f += 1;
        }
        while starts.get(s) == Some(at) {
            s += 1;
        }
        while ends.get(e) == Some(&at) {
            e += 1;
        }
        greatest = greatest.max(at);
        let (encoding, lsda) = match kind {
            Some(Kind::Compact { encoding, lsda }) => (encoding, lsda),
            Some(Kind::Dwarf { fde }) => (dwarf_encoding(dwarf_mode, fde), 0),
            None => (0, 0),
        };
        if previous == Some(encoding) && !lsda_last && lsda == 0 {
            continue;
        }
        let lsda_offset = match lsda {
            0 => None,
            lsda => Some(offset(base, lsda, "an LSDA")?),
        };
        let function = offset(base, at, "a function")?;
        if let Some(lsda) = lsda_offset {
            lsdas.push((function, lsda));
        }
        rows.push(Row { function, encoding });
        previous = Some(encoding);
        lsda_last = lsda_offset.is_some();
    }
    Ok(greatest)
}

const NO_COMMON: u8 = u8::MAX;

/// A distinct encoding: 16 bytes.
#[derive(Clone, Copy)]
struct Slot {
    encoding: u32,
    count: u32,
    /// The page being laid out ([`Encodings::stamp`]) that gave it an index of its own, and
    /// that index.
    page: u32,
    local: u8,
    common: u8,
    used: bool,
}

const _: () = assert!(size_of::<Slot>() == 16);

const EMPTY: Slot =
    Slot { encoding: 0, count: 0, page: 0, local: 0, common: NO_COMMON, used: false };

/// The distinct encodings but DWARF mode's, open-addressed with Fibonacci hashing and at most
/// half full: one multiply and about one probe a lookup. A DWARF-mode encoding names its FDE,
/// so it is of one entry: never common, new to its page, never looked up.
#[derive(Default)]
struct Encodings {
    slots: Vec<Slot>,
    len: usize,
    shift: u32,
    /// The page being laid out; a slot's `page` equal to it says its `local` is of this page.
    stamp: u32,
}

/// The fewest and the most slots [`Encodings::new`] starts with: 1 to 64 KiB, which holds 2048
/// distinct encodings, more than a large image has. Past that it doubles.
const SLOTS_MIN: usize = 64;
const SLOTS_MAX: usize = 4096;

impl Encodings {
    /// A table for `rows` second-level entries, which have at most that many distinct encodings.
    fn new(rows: usize) -> Self {
        let slots = (2 * rows).next_power_of_two().clamp(SLOTS_MIN, SLOTS_MAX);
        // alloc: once per link, sized so it seldom grows
        Self { slots: vec![EMPTY; slots], len: 0, shift: 32 - slots.trailing_zeros(), stamp: 0 }
    }

    /// The slot of `encoding`, or the empty one it goes in.
    fn find(&self, encoding: u32) -> usize {
        let mask = self.slots.len() - 1;
        let mut at = (encoding.wrapping_mul(0x9E37_79B9) >> self.shift) as usize;
        while self.slots[at].used && self.slots[at].encoding != encoding {
            at = (at + 1) & mask;
        }
        at
    }

    fn count(&mut self, encoding: u32) {
        let at = self.find(encoding);
        let slot = &mut self.slots[at];
        if slot.used {
            slot.count += 1;
            return;
        }
        *slot = Slot { encoding, count: 1, used: true, ..EMPTY };
        self.len += 1;
        if 2 * self.len > self.slots.len() {
            // alloc: only past `SLOTS_MAX / 2` distinct encodings, doubling
            let grown = vec![EMPTY; 2 * self.slots.len()];
            let old = std::mem::replace(&mut self.slots, grown);
            self.shift -= 1;
            for slot in old.into_iter().filter(|it| it.used) {
                let at = self.find(slot.encoding);
                self.slots[at] = slot;
            }
        }
    }

    /// One entry fewer of `encoding`, which was counted.
    fn uncount(&mut self, encoding: u32) {
        let at = self.find(encoding);
        let slot = &mut self.slots[at];
        debug_assert!(slot.used && slot.count > 0, "{encoding:#010x} was counted");
        slot.count = slot.count.saturating_sub(1);
    }

    /// The common encodings, into `common`: of those of more than one entry the most frequent,
    /// ties by value, at most 127. One of a single entry is no smaller common than the page's
    /// own. Each as (its count, the encoding).
    fn common(&mut self, common: &mut Vec<(u32, u32)>) {
        common.clear();
        let repeated = self.slots.iter().filter(|it| it.used && it.count > 1);
        common.extend(repeated.map(|it| (it.count, it.encoding)));
        radix::sort_copy_by_key(common, |&(count, encoding)| radix::pack_32(!count, encoding));
        common.truncate(COMMON_MAX);
        for (index, &(_, encoding)) in common.iter().enumerate() {
            let at = self.find(encoding);
            self.slots[at].common = index as u8;
        }
    }

    /// `encoding`'s index in the compressed page being laid out, and whether it is new to the
    /// page (then it takes `next`).
    fn index(&mut self, encoding: u32, dwarf: bool, next: usize) -> (u8, bool) {
        if dwarf {
            return (next as u8, true);
        }
        let stamp = self.stamp;
        let at = self.find(encoding);
        let slot = &mut self.slots[at];
        debug_assert!(slot.used, "every encoding but DWARF mode's is counted");
        if slot.common != NO_COMMON {
            return (slot.common, false);
        }
        if slot.page == stamp {
            return (slot.local, false);
        }
        slot.page = stamp;
        slot.local = next as u8;
        (slot.local, true)
    }

    /// The second-level pages, as lld splits them: as many entries as a compressed page holds
    /// (4096 bytes, function offsets within 24 bits of its first, 256 encoding indexes); a
    /// regular page instead where that is fewer than a regular page's 511 and more follow.
    fn pages(&mut self, rows: &[Row], common: usize, dwarf_mode: u32) -> Vec<Page> {
        let mut pages = Vec::with_capacity(rows.len() / PAGE_ENTRIES + 1);
        let mut i = 0;
        while i < rows.len() {
            let page = match dwarf_page(rows, i, common, dwarf_mode) {
                Some(page) => page,
                None => self.page(rows, i, common, dwarf_mode),
            };
            i += page.count;
            pages.push(page);
        }
        pages
    }

    /// The page [`Encodings::pages`] starts at `rows[first]`, its `at` unset.
    fn page(&mut self, rows: &[Row], first: usize, common: usize, dwarf_mode: u32) -> Page {
        let mut i = first;
        let limit = u64::from(rows[i].function) + u64::from(COMPRESSED_OFFSET_MASK);
        let mut words = (PAGE - COMPRESSED_HEADER) / 4;
        let mut next = common;
        self.stamp += 1;
        while words >= 1 && i < rows.len() {
            let row = rows[i];
            if u64::from(row.function) >= limit {
                break;
            }
            let dwarf = row.encoding & MODE_MASK == dwarf_mode;
            let (_, new) = self.index(row.encoding, dwarf, next);
            let cost = 1 + u64::from(new);
            if words < cost || (new && next == ENCODINGS_MAX) {
                break;
            }
            words -= cost;
            next += usize::from(new);
            i += 1;
        }
        let count = i - first;
        if i < rows.len() && count < PAGE_ENTRIES {
            let count = PAGE_ENTRIES.min(rows.len() - first);
            Page { first, count, compressed: false, locals: 0, at: 0 }
        } else {
            Page { first, count, compressed: true, locals: next - common, at: 0 }
        }
    }
}

/// The page [`Encodings::page`] starts at `rows[first]` when a regular page's worth of rows
/// from there are all DWARF mode's and more follow than a compressed page holds of them,
/// found without walking them one by one: each takes 8 bytes and an index of its own, so a
/// compressed page holds at most 510 by its bytes and `ENCODINGS_MAX - common` by its indexes,
/// fewer than a regular page's 511, so the page is regular. None otherwise (the last page,
/// whose 24-bit offsets [`Encodings::page`] checks, or one of other encodings).
fn dwarf_page(rows: &[Row], first: usize, common: usize, dwarf_mode: u32) -> Option<Page> {
    let left = rows.len() - first;
    let most = (((PAGE - COMPRESSED_HEADER) / COMPRESSED_ENTRY / 2) as usize).min(ENCODINGS_MAX.saturating_sub(common));
    if left <= most {
        return None;
    }
    let window = &rows[first..first + PAGE_ENTRIES.min(left)];
    // A fold, not an early exit, so it is vectorized: 4 KiB a page.
    let other = window.iter().fold(0, |acc, it| acc | ((it.encoding & MODE_MASK) ^ dwarf_mode));
    (other == 0).then_some(Page { first, count: PAGE_ENTRIES.min(left), compressed: false, locals: 0, at: 0 })
}

/// A second-level page: its rows, of a compressed one how many encodings of its own, and its
/// offset in the table.
#[derive(Clone, Copy)]
struct Page {
    first: usize,
    count: usize,
    compressed: bool,
    locals: usize,
    at: u64,
}

impl Page {
    fn len(&self) -> u64 {
        if self.compressed {
            COMPRESSED_HEADER + COMPRESSED_ENTRY * self.count as u64 + 4 * self.locals as u64
        } else {
            PAGE_HEADER + PAGE_ENTRY * self.count as u64
        }
    }
}

/// The table's bytes, sized exactly before anything is written, and where the next goes.
struct Out<'a> {
    bytes: &'a mut [u8],
    at: usize,
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

impl Out<'_> {
    fn put(&mut self, at: usize, value: u32) {
        put(self.bytes, at, value);
    }

    fn u32(&mut self, value: u32) {
        self.put(self.at, value);
        self.at += 4;
    }

    fn u16(&mut self, value: u16) {
        self.bytes[self.at..self.at + 2].copy_from_slice(&value.to_le_bytes());
        self.at += 2;
    }
}

/// Every FDE of `__eh_frame`, sorted by the start of the function it covers; of two at one
/// address, the first. One [`scan`] of the records (an FDE at 0 was of a dead function), in
/// parallel pieces split where the core placed every [`CHUNK`]th function's FDE. Into `keep`,
/// every record, the CIEs, where the scan stopped and whether two FDEs started at one address.
fn fdes(placed: &Placed<'_>, mut keep: Option<&mut Last>) -> Result<Vec<Fde>, LinkError> {
    if let Some(last) = keep.as_deref_mut() {
        last.records.clear();
        last.cies.clear();
        last.eh_end = 0;
        last.dups = false;
    }
    let Some((index, region)) = placed.region(&Class::EhFrame) else {
        // alloc: empty, which allocates nothing
        return Ok(Vec::new());
    };
    let bytes = region_bytes(placed, index, region, "__eh_frame")?;
    let section = region.addr;
    let splits = fde_splits(placed, section, bytes.len(), CHUNK);
    // alloc: error path
    let failed = |why: String| error(format!("__eh_frame at {section:#x}: {why}"));
    let mut fdes = match keep.as_deref_mut() {
        None => {
            let each = |at: usize, frame: Frame, fdes: &mut Vec<Fde>| {
                if let Frame::Fde { pc, len } = frame
                    && pc != 0
                {
                    let (addr, end) = covers(at, pc, len)?;
                    fdes.push(Fde { addr, end, offset: at as u64 });
                }
                Ok(())
            };
            scan(bytes, section, &splits, each).map_err(failed)?.items
        }
        Some(last) => {
            let each = |at: usize, frame: Frame, records: &mut Vec<Record>| {
                records.push(match frame {
                    Frame::Cie(_) => Record { at: at as u64, start: u64::MAX, end: 0 },
                    Frame::Fde { pc, len } => {
                        let (start, end) = covers(at, pc, len)?;
                        Record { at: at as u64, start, end }
                    }
                });
                Ok(())
            };
            let scan = scan(bytes, section, &splits, each).map_err(failed)?;
            last.records = scan.items;
            last.cies = scan.cies;
            last.eh_end = scan.end;
            live_fdes(&last.records)
        }
    };
    if !par::all(par::windows(&fdes, 2), |it| it[0].addr <= it[1].addr) {
        radix::sort_copy_by_key(&mut fdes, |it| it.addr);
    }
    let dups = par::any(par::windows(&fdes, 2), |it| it[0].addr == it[1].addr);
    if dups {
        fdes.dedup_by_key(|it| it.addr);
    }
    if let Some(last) = keep {
        last.dups = dups;
    }
    Ok(fdes)
}

/// Functions a piece of the `__eh_frame` scan reads the FDEs of, and FDE records a piece of
/// [`live_fdes`] turns into the column.
const CHUNK: usize = 4096;

/// The range an FDE at `at` covers from `pc`, `len` bytes: none (0..0) for a dead one.
fn covers(at: usize, pc: u64, len: u64) -> Result<(u64, u64), String> {
    if pc == 0 {
        return Ok((0, 0));
    }
    // alloc: error path
    let end = pc.checked_add(len).ok_or_else(|| format!("the FDE at offset {at:#x} covers past the end of the address space"))?;
    Ok((pc, end))
}

/// The column of the FDEs of `records` that cover something, in order: counted, then filled
/// at their place, a piece of [`CHUNK`] records a task.
fn live_fdes(records: &[Record]) -> Vec<Fde> {
    let live = |it: &&Record| !it.cie() && it.start != 0;
    // alloc: once per link, a count per piece
    let counts: Vec<usize> = par::map(par::chunks(records, CHUNK), |chunk| chunk.iter().filter(live).count());
    let len = counts.iter().sum();
    // alloc: once per link, the column, filled in parallel at each piece's place
    let mut fdes: Vec<Fde> = Vec::with_capacity(len);
    par::extend(&mut fdes, 0..len, |_| Fde::default());
    let mut rest = &mut fdes[..];
    // alloc: once per link, one slice per piece
    let mut jobs = Vec::with_capacity(counts.len());
    for (chunk, &count) in records.chunks(CHUNK).zip(&counts) {
        let (these, after) = std::mem::take(&mut rest).split_at_mut(count);
        jobs.push((these, chunk));
        rest = after;
    }
    par::for_each(jobs, |(out, chunk)| {
        for (out, it) in out.iter_mut().zip(chunk.iter().filter(live)) {
            *out = Fde { addr: it.start, end: it.end, offset: it.at };
        }
    });
    fdes
}

/// A record of `__eh_frame`: the terminator, a CIE with its FDE pointer encoding, or an FDE
/// with the range it covers (0..0 for a dead one).
enum Parsed {
    End,
    Cie(u8),
    Fde { start: u64, end: u64 },
}

/// The record at offset `record` of `__eh_frame` (`bytes`, at address `section`), read as
/// [`scan`] reads it, and the offset after it; `cies` holds the CIEs, by offset. The record is
/// read in place, or put together in `across` where it crosses a page.
fn record(
    bytes: View<'_>,
    record: usize,
    section: u64,
    cies: &[(usize, u8)],
    across: &mut Vec<u8>,
) -> Result<(Parsed, usize), LinkError> {
    let bad = |what: &str| {
        // alloc: error path
        error(format!("__eh_frame at {section:#x}: the entry at offset {record:#x}: {what}"))
    };
    let Some(found) = entry(bytes, record).map_err(bad)? else {
        return Ok((Parsed::End, record));
    };
    let cie = |named: usize| -> Result<u8, &'static str> {
        let found = cies.binary_search_by_key(&named, |it| it.0).map_err(|_| "its CIE pointer names no CIE")?;
        Ok(cies[found].1)
    };
    let parsed = match frame(bytes, record, found, section, across, cie).map_err(bad)? {
        Frame::Cie(encoding) => Parsed::Cie(encoding),
        Frame::Fde { pc, len } => {
            // alloc: error path
            let (start, end) = covers(record, pc, len).map_err(|it| error(format!("__eh_frame at {section:#x}: {it}")))?;
            Parsed::Fde { start, end }
        }
    };
    Ok((parsed, found.1))
}

fn region_bytes<'p>(placed: &Placed<'p>, index: usize, region: &Region, name: &str) -> Result<View<'p>, LinkError> {
    let bytes = placed.view(index);
    usize::try_from(region.size).ok().and_then(|size| bytes.prefix(size)).ok_or_else(|| {
        // alloc: error path
        error(format!(
            "{name} at {:#x}: {} bytes placed, {} in its contents",
            region.addr,
            region.size,
            bytes.len()
        ))
    })
}

/// `addr`'s offset from the image's first byte, which every offset in the table is.
fn offset(base: u64, addr: u64, what: &str) -> Result<u32, LinkError> {
    addr.checked_sub(base).and_then(|it| u32::try_from(it).ok()).ok_or_else(|| {
        // alloc: error path
        error(format!(
            "__unwind_info: {what} at {addr:#x} is not within 4 GiB above the image's base \
             {base:#x}"
        ))
    })
}

/// The function at `addr`, for an error: called only to build one.
fn named(placed: &Placed<'_>, addr: u64) -> String {
    named_in(placed.functions, addr)
}

/// The function of `functions` at `addr`, for an error.
fn named_in(functions: &[Function], addr: u64) -> String {
    match functions.iter().find(|it| it.addr == addr).and_then(|it| it.name.as_deref()) {
        // alloc: error path
        Some(name) => format!("`{name}` at {addr:#x}"),
        // alloc: error path
        None => format!("the function at {addr:#x}"),
    }
}

fn error(what: String) -> LinkError {
    LinkError { function: None, what, refusal: None }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use blaze_codegen::Arch;
    use blaze_ids::rng::SplitMix64 as Rng;
    use gimli::read::{BaseAddresses, CieOrFde, UnwindSection};
    use gimli::write::{
        Address, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry, FrameTable,
    };

    use super::{
        ARM64_MODE_DWARF, COMMON_MAX, COMPACT_ENTRY, DWARF_OFFSET_MASK, HAS_LSDA, HEADER,
        Held, INDEX_ENTRY, LSDA_ENTRY, MODE_MASK, PAGE_ENTRY, PAGE_HEADER, PERSONALITY_MASK,
        PERSONALITY_MAX, PERSONALITY_SHIFT, SECOND_LEVEL_REGULAR, VERSION, X86_64_MODE_DWARF,
        Last, bound, build, error, kept, named, offset, region_bytes, u32_at, u64_at,
    };
    use crate::LinkError;
    use crate::model::{Bind, Class, Delta, Function, Placed, Plan, Prot, Region, Splice, View};
    use crate::output::{Debug, Format, Output, OutputKind};

    const BASE: u64 = 0x1_0000_0000;
    const TEXT: u64 = BASE + 0x4000;
    const EH_FRAME: u64 = BASE + 0x8000;
    const COMPACT: u64 = BASE + 0xc000;
    const LSDA: u64 = BASE + 0x9000;
    const PERSONALITY: u64 = TEXT + 0x800;
    const FRAME: u32 = 0x0400_0000;
    const FRAMELESS: u32 = 0x0200_1000;

    /// A test region's bytes, which are one slice (`paged` is empty).
    fn flat(view: View<'_>) -> &[u8] {
        match view {
            View::Slice(bytes) => bytes,
            View::Pages(_) => panic!("a test region in pages"),
        }
    }

    fn output(arch: Arch) -> Output {
        Output {
            format: Format::MachO,
            arch,
            kind: OutputKind::Executable { entry: "main".into() },
            interpreter: None,
            platform: None,
            incremental: false,
            debug: Debug::None,
            live: None,
            tags: 0,
            stack: None,
            windows_subsystem: false,
            dev_run: None,
            exec_log: false,
            sanitize: blaze_codegen::settings::Sanitize::default(),
            rpaths: Vec::new(),
            triple: match arch { Arch::X86_64 => "x86_64-apple-darwin", _ => "aarch64-apple-darwin" }.into(),
        }
    }

    fn plan() -> Plan {
        Plan {
            base: BASE,
            headers: 0x4000,
            page: 0x4000,
            indexed_code: false,
            got_by_library: false,
            tls_descriptors: true,
        }
    }

    fn region(class: Class, addr: u64, size: u64) -> Region {
        Region { class, addr, capacity: size, size, prot: Prot::R, align: 8 }
    }

    fn compact_region(bytes: &[u8]) -> Region {
        region(super::super::compact_unwind(), COMPACT, bytes.len() as u64)
    }

    fn function(addr: u64, size: u64, name: &str) -> Function {
        Function { addr, size, fde: None, windows: None, name: Some(name.into()) }
    }

    fn no_symbol(_: &str) -> Option<u64> {
        None
    }

    /// An `__eh_frame` at [`EH_FRAME`] of one CIE and an FDE for each range, its addresses
    /// pc-relative; and each FDE's offset in it.
    fn eh_frame(ranges: &[(u64, u32)]) -> (Vec<u8>, Vec<u64>) {
        let encoding =
            gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
        let mut cie = CommonInformationEntry::new(encoding, 4, -8, gimli::Register(30));
        cie.fde_address_encoding = gimli::DW_EH_PE_pcrel | gimli::DW_EH_PE_sdata4;
        let mut table = FrameTable::default();
        let cie = table.add_cie(cie);
        for &(addr, len) in ranges {
            let start = Address::Constant(addr.wrapping_sub(EH_FRAME));
            table.add_fde(cie, FrameDescriptionEntry::new(start, len));
        }
        let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
        table.write_eh_frame(&mut section).unwrap();
        let bytes = section.0.into_vec();
        let offsets = fde_offsets(&bytes);
        (bytes, offsets)
    }

    /// The offset of each FDE in `bytes`, in order.
    fn fde_offsets(bytes: &[u8]) -> Vec<u64> {
        let mut offsets = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let length = u32_at(bytes, at) as usize;
            if u32_at(bytes, at + 4) != 0 {
                offsets.push(at as u64);
            }
            at += 4 + length;
        }
        offsets
    }

    fn compact(entries: &[(u64, u32, u32, u64, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(function, length, encoding, personality, lsda) in entries {
            bytes.extend_from_slice(&function.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(&encoding.to_le_bytes());
            bytes.extend_from_slice(&personality.to_le_bytes());
            bytes.extend_from_slice(&lsda.to_le_bytes());
        }
        bytes
    }

    /// The table as libunwind reads it, both kinds of second-level page.
    struct Table<'a> {
        bytes: &'a [u8],
    }

    impl Table<'_> {
        fn word(&self, at: u32) -> u32 {
            u32_at(self.bytes, at as usize)
        }

        fn half(&self, at: u32) -> u16 {
            u16::from_le_bytes(self.bytes[at as usize..at as usize + 2].try_into().unwrap())
        }

        fn personalities(&self) -> Vec<u32> {
            (0..self.word(16)).map(|i| self.word(self.word(12) + 4 * i)).collect()
        }

        fn common(&self) -> Vec<u32> {
            (0..self.word(8)).map(|i| self.word(self.word(4) + 4 * i)).collect()
        }

        /// The `i`th first-level entry: function offset, page offset, LSDA index offset.
        fn first_level(&self, i: u32) -> (u32, u32, u32) {
            let at = self.word(20) + 12 * i;
            (self.word(at), self.word(at + 4), self.word(at + 8))
        }

        fn index(&self) -> Vec<(u32, u32, u32)> {
            (0..self.word(24)).map(|i| self.first_level(i)).collect()
        }

        /// The kind of each second-level page.
        fn kinds(&self) -> Vec<u32> {
            (0..self.word(24) - 1).map(|i| self.word(self.first_level(i).1)).collect()
        }

        fn count(&self, page: u32) -> u32 {
            u32::from(self.half(page + 6))
        }

        /// The `i`th entry of the page at `page`, whose first-level function offset is
        /// `first`: its function offset and encoding.
        fn entry(&self, page: u32, first: u32, i: u32) -> (u32, u32) {
            let start = page + u32::from(self.half(page + 4));
            match self.word(page) {
                2 => (self.word(start + 8 * i), self.word(start + 8 * i + 4)),
                3 => {
                    let entry = self.word(start + 4 * i);
                    let index = entry >> 24;
                    let common = self.word(8);
                    let encoding = if index < common {
                        self.word(self.word(4) + 4 * index)
                    } else {
                        let local = index - common;
                        assert!(local < u32::from(self.half(page + 10)), "a local index in range");
                        self.word(page + u32::from(self.half(page + 8)) + 4 * local)
                    };
                    (first + (entry & 0x00ff_ffff), encoding)
                }
                kind => panic!("a second-level page of kind {kind}"),
            }
        }

        /// Every second-level entry, in order.
        fn entries(&self) -> Vec<(u32, u32)> {
            let mut entries = Vec::new();
            for i in 0..self.word(24) - 1 {
                let (first, page, _) = self.first_level(i);
                assert_eq!(self.entry(page, first, 0).0, first, "a page starts at its index entry");
                for n in 0..self.count(page) {
                    entries.push(self.entry(page, first, n));
                }
            }
            entries
        }

        /// The LSDA index: function and LSDA offsets.
        fn lsdas(&self) -> Vec<(u32, u32)> {
            let (from, to) = (self.first_level(0).2, self.first_level(self.word(24) - 1).2);
            (from..to).step_by(8).map(|at| (self.word(at), self.word(at + 4))).collect()
        }

        /// What libunwind finds for `pc`, an offset from the base: the entry's function, its
        /// encoding and, when the encoding has one, the LSDA's offset.
        fn find(&self, pc: u32) -> Option<(u32, u32, Option<u32>)> {
            let pages = self.word(24).checked_sub(1)?;
            let page = (0..pages)
                .find(|&i| self.first_level(i).0 <= pc && pc < self.first_level(i + 1).0)?;
            let (first, at, lsda_from) = self.first_level(page);
            let lsda_to = self.first_level(page + 1).2;
            let (mut low, mut high) = (0, self.count(at));
            while low < high {
                let middle = (low + high) / 2;
                if self.entry(at, first, middle).0 <= pc {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            let (function, encoding) = self.entry(at, first, low.checked_sub(1)?);
            if encoding & HAS_LSDA == 0 {
                return Some((function, encoding, None));
            }
            let lsda = (lsda_from..lsda_to)
                .step_by(8)
                .find(|&at| self.word(at) == function)
                .map(|at| self.word(at + 4));
            assert!(lsda.is_some(), "an LSDA for the function at {function:#x}");
            Some((function, encoding, lsda))
        }

        fn lookup(&self, pc: u32) -> Option<(u32, Option<u32>)> {
            self.find(pc).map(|(_, encoding, lsda)| (encoding, lsda))
        }
    }

    fn off(addr: u64) -> u32 {
        (addr - BASE) as u32
    }

    #[test]
    fn each_function_gets_its_encoding() {
        let f0 = TEXT;
        let f1 = TEXT + 0x40;
        let f2 = TEXT + 0x80;
        let f3 = TEXT + 0xc0;
        let f4 = TEXT + 0xd0;
        let f5 = TEXT + 0x100;
        let f6 = TEXT + 0x110;
        let f7 = TEXT + 0x200;
        let (eh, fde) = eh_frame(&[(f0, 0x40), (f2, 0x20), (f1, 0x40), (f4, 0x30)]);
        let cu = compact(&[
            (f1, 0x40, FRAME, PERSONALITY, LSDA),
            (0, 0x40, FRAME, PERSONALITY, LSDA + 0x40),
            (f4, 0x30, ARM64_MODE_DWARF, 0, 0),
            (f5, 0x10, FRAMELESS, PERSONALITY, 0),
            (f6, 0x10, FRAMELESS, PERSONALITY, 0),
        ]);
        let regions = [
            region(Class::Text, TEXT, 0x1000),
            region(Class::EhFrame, EH_FRAME, eh.len() as u64),
            compact_region(&cu),
        ];
        let contents: [&[u8]; 3] = [&[], &eh, &cu];
        let functions = [
            function(f0, 0x40, "f0"),
            function(f1, 0x40, "f1"),
            function(f2, 0x20, "f2"),
            function(f3, 0x10, "f3"),
            function(f4, 0x30, "f4"),
            function(f5, 0x10, "f5"),
            function(f6, 0x10, "f6"),
            function(f7, 0x10, "f7"),
        ];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = Placed {
            output: &output,
            name: "test",
            plan: &plan,
            regions: &regions,
            contents: &contents,
            paged: &[],
            libraries: &[],
            imports: &[],
            got: &[],
            rebase: &[COMPACT + 16, COMPACT + 32 * 3 + 16, COMPACT + 32 * 4 + 16],
            binds: &[],
            exports: &[],
            functions: &functions,
            entry: None,
            init: None,
            fini: None,
            tls: None,
            id: [0; 16],
            debug: &[],
            symbol: &no_symbol,
            synthesis: 0,
            delta: None,
            encoded: &[],
        };
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(functions.len() as u64, 8, cu.len() as u64));
        let table = Table { bytes: &bytes };
        assert_eq!(table.word(0), 1, "version 1");
        assert_eq!(table.kinds(), vec![3], "one compressed page");

        assert_eq!(table.personalities(), vec![off(COMPACT + 16)], "one personality, by its cell");
        let personality = 1 << 28;
        let dwarf = |n: usize| ARM64_MODE_DWARF | fde[n] as u32;

        assert_eq!(table.lookup(off(f0)), Some((dwarf(0), None)));
        assert_eq!(table.lookup(off(f0) + 0x3c), Some((dwarf(0), None)));
        assert_eq!(
            table.lookup(off(f1)),
            Some((FRAME | personality | HAS_LSDA, Some(off(LSDA)))),
            "the compact entry wins over the FDE of the same function"
        );
        assert_eq!(table.lookup(off(f2)), Some((dwarf(1), None)));
        assert_eq!(table.lookup(off(f2) + 0x20), Some((0, None)), "the end marker after f2");
        assert_eq!(table.lookup(off(f3)), Some((0, None)));
        assert_eq!(table.lookup(off(f4)), Some((dwarf(3), None)), "a DWARF-mode compact entry");
        assert_eq!(table.lookup(off(f5)), Some((FRAMELESS | personality, None)));
        assert_eq!(table.lookup(off(f6) + 4), Some((FRAMELESS | personality, None)));
        assert_eq!(table.lookup(off(f6) + 0x10), Some((0, None)), "the end marker after f6");
        assert_eq!(table.lookup(off(f7)), Some((0, None)));
        assert_eq!(table.lookup(off(f7) + 0x10), None, "past the sentinel");

        let entries = table.entries();
        assert!(entries.windows(2).all(|it| it[0].0 < it[1].0), "sorted, no duplicates");
        assert!(
            entries.windows(2).all(|it| it[0].1 != it[1].1 || it[1].1 & HAS_LSDA != 0),
            "consecutive entries with one encoding merged"
        );
        assert!(!entries.iter().any(|it| it.0 == off(f6)), "f6 merged into f5");
        assert!(entries.iter().all(|it| it.0 != 0), "the dead-stripped entry left out");
        assert!(
            entries.iter().all(|it| matches!(it.1 & PERSONALITY_MASK, 0 | 0x1000_0000)),
            "one personality, index 1"
        );

        let index = table.index();
        assert_eq!(index.len(), 2, "one page and the sentinel");
        assert_eq!(index[0].0, off(f0));
        let sentinel = index[1];
        assert_eq!(sentinel.0, off(f7) + 0x10, "the sentinel is the end of the last function");
        assert_eq!(sentinel.1, 0);
        assert_eq!(sentinel.2 - index[0].2, 8, "one LSDA entry");
        assert_eq!(table.word(index[0].2), off(f1));
        assert_eq!(table.word(index[0].2 + 4), off(LSDA));

        let common = table.common();
        assert!(common.len() <= 127);
        assert_eq!(common[0], 0, "encoding 0 is the most frequent");
    }

    #[test]
    fn x86_64_dwarf_mode() {
        let (eh, fde) = eh_frame(&[(TEXT, 0x10), (TEXT + 0x10, 0x10)]);
        let regions =
            [region(Class::Text, TEXT, 0x20), region(Class::EhFrame, EH_FRAME, eh.len() as u64)];
        let contents: [&[u8]; 2] = [&[], &eh];
        let functions = [function(TEXT, 0x10, "a"), function(TEXT + 0x10, 0x10, "b")];
        let output = output(Arch::X86_64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::X86_64).unwrap();
        assert!(bytes.len() as u64 <= bound(2, 2, 0));
        let table = Table { bytes: &bytes };
        let dwarf = |n: usize| Some((X86_64_MODE_DWARF | fde[n] as u32, None));
        assert_eq!(table.lookup(off(TEXT)), dwarf(0));
        assert_eq!(table.lookup(off(TEXT) + 0x18), dwarf(1));
        assert_eq!(table.lookup(off(TEXT) + 0x20), None);
        assert!(table.personalities().is_empty());
        assert!(table.common().is_empty(), "DWARF-mode encodings are of one entry each");
    }

    #[test]
    fn many_functions_span_pages() {
        let functions: Vec<Function> =
            (0..2000).map(|i| function(TEXT + 0x10 * i, 0x10, "f")).collect();
        let encoding = |i: u64| FRAMELESS + (i as u32 % 2) * 0x1000;
        let lsda = |i: u64| if i % 3 == 0 { LSDA + i } else { 0 };
        let entries: Vec<(u64, u32, u32, u64, u64)> =
            (0..2000).map(|i| (TEXT + 0x10 * i, 0x10, encoding(i), 0, lsda(i))).collect();
        let cu = compact(&entries);
        let regions = [region(Class::Text, TEXT, 0x10 * 2000), compact_region(&cu)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(functions.len() as u64, 0, cu.len() as u64));
        let table = Table { bytes: &bytes };
        assert!(table.index().len() > 2, "more than one page");
        assert!(table.kinds().iter().all(|it| *it == 3), "every page compressed");
        assert_eq!(table.common().len(), 4, "two encodings, each with and without an LSDA");
        assert!(bytes.len() < 2001 * 4 + 2000 / 3 * 8 + 256, "4 bytes an entry: {}", bytes.len());
        for i in (0..2000).step_by(7) {
            let lsda = (i % 3 == 0).then(|| off(LSDA + i));
            let encoding = encoding(i) | if lsda.is_some() { HAS_LSDA } else { 0 };
            let found = table.lookup(off(TEXT + 0x10 * i) + 4);
            assert_eq!(found, Some((encoding, lsda)), "function {i}");
        }
    }

    #[test]
    fn a_page_past_24_bits_of_function_offsets_ends() {
        let far = TEXT + 0x0100_0000;
        let functions = [function(TEXT, 0x10, "near"), function(far, 0x10, "far")];
        let cu = compact(&[(TEXT, 0x10, FRAME, 0, 0), (far, 0x10, FRAMELESS, 0, 0)]);
        let regions = [compact_region(&cu)];
        let contents: [&[u8]; 1] = [&cu];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        let table = Table { bytes: &bytes };
        assert_eq!(table.kinds(), vec![2], "regular, as a compressed page would hold 2 entries");
        assert_eq!(table.lookup(off(TEXT) + 4), Some((FRAME, None)));
        assert_eq!(table.lookup(off(TEXT) + 0x10), Some((0, None)));
        assert_eq!(table.lookup(off(far) + 4), Some((FRAMELESS, None)));
    }

    /// A program of more than 16 MiB of frames (bun's base, #173, has 20 MiB): an FDE past
    /// DWARF mode's 24-bit offset is DWARF mode with offset 0, which libunwind scans
    /// `__eh_frame` from the start for, and one within keeps its offset.
    #[test]
    fn an_fde_past_24_bits_of_eh_frame_is_dwarf_mode_from_the_start() {
        let mut eh = record(&[0, 0, 0, 0, 1, 0, 1, 0x78, 30]);
        let mut padded = [TEXT.to_le_bytes(), 0x10u64.to_le_bytes()].concat();
        // DW_CFA_nop, enough to take the next FDE past the field.
        padded.resize(padded.len() + DWARF_OFFSET_MASK as usize + 1, 0);
        push_fde(&mut eh, 0, &padded);
        push_fde(&mut eh, 0, &[(TEXT + 0x10).to_le_bytes(), 0x10u64.to_le_bytes()].concat());
        let fde = fde_offsets(&eh);
        assert!(fde[0] <= u64::from(DWARF_OFFSET_MASK) && fde[1] > u64::from(DWARF_OFFSET_MASK));
        let regions = [region(Class::Text, TEXT, 0x20), region(Class::EhFrame, EH_FRAME, eh.len() as u64)];
        let contents: [&[u8]; 2] = [&[], &eh];
        let functions = [function(TEXT, 0x10, "near"), function(TEXT + 0x10, 0x10, "far")];
        for (arch, mode) in [(Arch::Aarch64, ARM64_MODE_DWARF), (Arch::X86_64, X86_64_MODE_DWARF)] {
            let output = output(arch);
            let plan = plan();
            let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
            let bytes = build(&placed, arch).unwrap();
            let table = Table { bytes: &bytes };
            assert_eq!(table.lookup(off(TEXT) + 4), Some((mode | fde[0] as u32, None)), "{arch:?}");
            assert_eq!(table.lookup(off(TEXT) + 0x14), Some((mode, None)), "{arch:?}");
            assert_eq!(table.lookup(off(TEXT) + 0x20), None, "{arch:?}");
        }
    }

    #[test]
    fn a_bound_personality_is_its_import() {
        let cu = compact(&[(TEXT, 0x10, FRAME, 0, 0), (TEXT + 0x10, 0x10, FRAME, 0, 0)]);
        let regions = [region(Class::Text, TEXT, 0x20), compact_region(&cu)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let functions = [function(TEXT, 0x10, "a"), function(TEXT + 0x10, 0x10, "b")];
        let binds = [
            Bind { addr: COMPACT + 16, import: 3, addend: 0 },
            Bind { addr: COMPACT + 32 + 16, import: 3, addend: 0 },
        ];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &binds);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        let table = Table { bytes: &bytes };
        assert_eq!(table.personalities(), vec![off(COMPACT + 16)]);
        assert_eq!(table.lookup(off(TEXT + 0x10)), Some((FRAME | (1 << 28), None)));
    }

    #[test]
    fn four_personalities_are_an_error() {
        let cu = compact(&[
            (TEXT, 0x10, FRAME, PERSONALITY, 0),
            (TEXT + 0x10, 0x10, FRAME, PERSONALITY + 0x10, 0),
            (TEXT + 0x20, 0x10, FRAME, PERSONALITY + 0x20, 0),
            (TEXT + 0x30, 0x10, FRAME, PERSONALITY + 0x30, 0),
        ]);
        let regions = [region(Class::Text, TEXT, 0x40), compact_region(&cu)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let functions: Vec<Function> =
            (0..4).map(|i| function(TEXT + 0x10 * i, 0x10, "f")).collect();
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let error = build(&placed, Arch::Aarch64).unwrap_err();
        assert!(error.what.contains("fourth distinct personality"), "{}", error.what);
        assert!(error.what.contains(&format!("{:#x}", TEXT + 0x30)), "{}", error.what);
    }

    #[test]
    fn dwarf_mode_without_an_fde_is_an_error() {
        let (eh, _) = eh_frame(&[(TEXT, 0x10)]);
        let cu = compact(&[(TEXT + 0x10, 0x10, ARM64_MODE_DWARF, 0, 0)]);
        let regions = [
            region(Class::Text, TEXT, 0x20),
            region(Class::EhFrame, EH_FRAME, eh.len() as u64),
            compact_region(&cu),
        ];
        let contents: [&[u8]; 3] = [&[], &eh, &cu];
        let functions = [function(TEXT, 0x10, "a"), function(TEXT + 0x10, 0x10, "b")];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let error = build(&placed, Arch::Aarch64).unwrap_err();
        assert!(error.what.contains("DWARF mode"), "{}", error.what);
        assert!(error.what.contains(&format!("`b` at {:#x}", TEXT + 0x10)), "{}", error.what);
    }

    #[test]
    fn nothing_to_unwind() {
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &[], &[], &[], &[]);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(0, 0, 0));
        let table = Table { bytes: &bytes };
        assert_eq!(table.index().len(), 1, "the sentinel alone");
        assert!(table.entries().is_empty());
    }

    /// What changed since synthesis `from`: `functions`' splices and the `written` ranges.
    fn delta<'a>(from: u64, functions: &'a [Splice], written: &'a [(u64, u64)]) -> Delta<'a> {
        Delta {
            functions,
            rebase: &[],
            got: &[],
            binds: &[],
            same_names: true,
            same_regions: true,
            written,
            from,
        }
    }

    /// Ten functions with compact entries, each followed by a gap but the fourth, which ends
    /// where the fifth's gap starts, and an eleventh covered by an FDE.
    struct Image {
        functions: Vec<Function>,
        eh: Vec<u8>,
        cu: Vec<u8>,
    }

    /// The fourth function at `fourth`, the seventh's LSDA at `lsda`, the eleventh `size` long.
    fn image(fourth: u64, lsda: u64, size: u64) -> Image {
        let mut functions = Vec::with_capacity(11);
        let mut entries = Vec::with_capacity(10);
        for i in 0..10u64 {
            let (addr, len) =
                if i == 3 { (fourth, TEXT + 0xf0 - fourth) } else { (TEXT + 0x40 * i, 0x30) };
            functions.push(function(addr, len, "f"));
            let encoding = if i % 2 == 0 { FRAME } else { FRAMELESS };
            let lsda = match i {
                6 => lsda,
                i if i % 3 == 0 => LSDA + 0x10 * i,
                _ => 0,
            };
            entries.push((addr, len as u32, encoding, 0, lsda));
        }
        let eleventh = TEXT + 0x280;
        functions.push(function(eleventh, size, "eleventh"));
        let (eh, _) = eh_frame(&[(eleventh, size as u32)]);
        Image { functions, eh, cu: compact(&entries) }
    }

    fn image_regions(image: &Image) -> [Region; 3] {
        [
            region(Class::Text, TEXT, 0x400),
            region(Class::EhFrame, EH_FRAME, image.eh.len() as u64),
            compact_region(&image.cu),
        ]
    }

    /// `image` linked first, by synthesis `synthesis`, into `last`.
    fn first_link(image: &Image, synthesis: u64, last: &mut Last) {
        let output = output(Arch::Aarch64);
        let plan = plan();
        let regions = image_regions(image);
        let contents: [&[u8]; 3] = [&[], &image.eh, &image.cu];
        let placed = placed(&output, &plan, &regions, &contents, &image.functions, &[]);
        let placed = Placed { synthesis, ..placed };
        let (bytes, patched) = kept(&placed, Arch::Aarch64, last).unwrap();
        assert!(!patched, "a link without a delta builds whole");
        assert_eq!(&bytes.contiguous()[..], &build(&placed, Arch::Aarch64).unwrap()[..]);
    }

    /// `image` linked again by synthesis `synthesis` with `delta`: the table and whether it was
    /// patched, the table checked against `build`'s.
    fn relink(
        image: &Image,
        synthesis: u64,
        delta: &Delta<'_>,
        last: &mut Last,
    ) -> (Vec<u8>, bool) {
        let output = output(Arch::Aarch64);
        let plan = plan();
        let regions = image_regions(image);
        let contents: [&[u8]; 3] = [&[], &image.eh, &image.cu];
        let placed = placed(&output, &plan, &regions, &contents, &image.functions, &[]);
        let placed = Placed { synthesis, delta: Some(delta), ..placed };
        let (bytes, patched) = kept(&placed, Arch::Aarch64, last).unwrap();
        let bytes = bytes.contiguous().into_owned();
        assert_eq!(bytes, build(&placed, Arch::Aarch64).unwrap(), "the bytes `build` makes");
        (bytes, patched)
    }

    #[test]
    fn a_relink_patches_moved_functions_and_lsdas_as_build_makes_them() {
        let mut last = Last::default();
        let first = image(TEXT + 0xc0, LSDA + 0x60, 0x30);
        first_link(&first, 7, &mut last);

        // The fourth function moves up its gap and shrinks, the seventh's LSDA moves, and the
        // eleventh, the last, grows with its FDE: the sentinel moves.
        let second = image(TEXT + 0xc8, LSDA + 0x68, 0x40);
        assert_eq!(second.eh.len(), first.eh.len(), "an FDE of the same length");
        let splices = [Splice { old: 3..4, new: 3..4 }, Splice { old: 10..11, new: 10..11 }];
        let written =
            [(EH_FRAME, second.eh.len() as u64), (COMPACT + 32 * 3, 32), (COMPACT + 32 * 6, 32)];
        let (bytes, patched) = relink(&second, 8, &delta(7, &splices, &written), &mut last);
        assert!(patched, "every row kept its place");
        let table = Table { bytes: &bytes };
        assert_eq!(table.lookup(off(TEXT + 0xc4)), Some((0, None)), "the gap before the fourth");
        assert_eq!(
            table.lookup(off(TEXT + 0x180)),
            Some((FRAME | HAS_LSDA, Some(off(LSDA + 0x68)))),
            "the seventh's new LSDA"
        );
        let grown = table.lookup(off(TEXT + 0x2b8)).unwrap().0;
        assert_eq!(grown & MODE_MASK, ARM64_MODE_DWARF, "the eleventh's FDE covers what it grew");
        assert_eq!(table.index().last().unwrap().0, off(TEXT + 0x2c0), "the sentinel moved");

        // And back, from the patched table, but for the eleventh.
        let third = image(TEXT + 0xc0, LSDA + 0x60, 0x40);
        let splices = [Splice { old: 3..4, new: 3..4 }];
        let written = [(COMPACT + 32 * 3, 32), (COMPACT + 32 * 6, 32)];
        let (_, patched) = relink(&third, 9, &delta(8, &splices, &written), &mut last);
        assert!(patched, "a patched table patches again");
    }

    #[test]
    fn a_delta_from_another_synthesis_builds_whole() {
        let mut last = Last::default();
        first_link(&image(TEXT + 0xc0, LSDA + 0x60, 0x30), 7, &mut last);
        let second = image(TEXT + 0xc8, LSDA + 0x60, 0x30);
        let splices = [Splice { old: 3..4, new: 3..4 }];
        let written = [(COMPACT + 32 * 3, 32)];
        let (_, patched) = relink(&second, 8, &delta(6, &splices, &written), &mut last);
        assert!(!patched, "the table is not of the synthesis the delta is from");

        // Built whole, it is this link's: the next delta, from it, patches.
        let third = image(TEXT + 0xc4, LSDA + 0x60, 0x30);
        let (_, patched) = relink(&third, 9, &delta(8, &splices, &written), &mut last);
        assert!(patched);
    }

    #[test]
    fn an_unequal_splice_builds_whole() {
        let mut last = Last::default();
        let first = image(TEXT + 0xc0, LSDA + 0x60, 0x30);
        first_link(&first, 7, &mut last);
        let mut second = image(TEXT + 0xc0, LSDA + 0x60, 0x30);
        second.functions.push(function(TEXT + 0x300, 0x10, "new"));
        let splices = [Splice { old: 11..11, new: 11..12 }];
        let (_, patched) = relink(&second, 8, &delta(7, &splices, &[]), &mut last);
        assert!(!patched, "a function more");
    }

    #[test]
    fn a_change_of_the_rows_builds_whole() {
        let mut last = Last::default();
        first_link(&image(TEXT + 0xc0, LSDA + 0x60, 0x30), 7, &mut last);
        // The fourth now starts where the third ends: the gap's row goes.
        let second = image(TEXT + 0xb0, LSDA + 0x60, 0x30);
        let splices = [Splice { old: 3..4, new: 3..4 }];
        let written = [(COMPACT + 32 * 3, 32)];
        let (_, patched) = relink(&second, 8, &delta(7, &splices, &written), &mut last);
        assert!(!patched, "a row fewer");

        // Nor does an LSDA appear where there was none.
        let mut third = image(TEXT + 0xb0, LSDA + 0x60, 0x30);
        third.cu[32 * 4 + 24..32 * 5].copy_from_slice(&(LSDA + 0x40).to_le_bytes());
        let written = [(COMPACT + 32 * 4, 32)];
        let (_, patched) = relink(&third, 9, &delta(8, &[], &written), &mut last);
        assert!(!patched, "an LSDA more");
    }

    #[test]
    fn moved_lsdas_patch_as_build_makes_them() {
        let mut patches = 0;
        for seed in 0..32 {
            let case = case(seed);
            let output = output(case.arch);
            let plan = plan();
            let regions = [
                region(Class::EhFrame, EH_FRAME, case.eh.len() as u64),
                compact_region(&case.cu),
            ];
            let contents: [&[u8]; 2] = [&case.eh, &case.cu];
            let first = placed(&output, &plan, &regions, &contents, &case.functions, &case.binds);
            let first = Placed { synthesis: 1, ..first };
            let mut last = Last::default();
            let Ok((bytes, _)) = kept(&first, case.arch, &mut last) else {
                continue;
            };
            assert_eq!(&bytes.contiguous()[..], &build(&first, case.arch).unwrap()[..], "seed {seed}");

            let mut rng = Rng::new(seed ^ 0x5eed);
            let mut cu = case.cu.clone();
            let mut written = Vec::new();
            for n in 0..cu.len() / 32 {
                let entry = &mut cu[32 * n..32 * n + 32];
                if u64_at(entry, 0) != 0 && u64_at(entry, 24) != 0 && rng.below(2) == 0 {
                    let lsda = u64_at(entry, 24) + 4;
                    entry[24..].copy_from_slice(&lsda.to_le_bytes());
                    written.push((COMPACT + 32 * n as u64, 32));
                }
            }
            let contents: [&[u8]; 2] = [&case.eh, &cu];
            let delta = delta(1, &[], &written);
            let second = placed(&output, &plan, &regions, &contents, &case.functions, &case.binds);
            let second = Placed { synthesis: 2, delta: Some(&delta), ..second };
            let (bytes, patched) = kept(&second, case.arch, &mut last).unwrap();
            assert_eq!(&bytes.contiguous()[..], &build(&second, case.arch).unwrap()[..], "seed {seed}");
            patches += usize::from(patched);
        }
        assert!(patches > 0, "some relinks patched");
    }

    /// The bytes of a CIE or FDE whose body is `body`.
    fn record(body: &[u8]) -> Vec<u8> {
        let mut record = (body.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(body);
        record
    }

    /// An FDE appended to `eh` naming the CIE at `cie`, its body after the CIE pointer `rest`.
    fn push_fde(eh: &mut Vec<u8>, cie: usize, rest: &[u8]) {
    let pointer = (eh.len() + 4 - cie) as u32;
    let parts: [&[u8]; 2] = [&pointer.to_le_bytes(), rest];
    let record = record(&parts.concat());
        eh.extend_from_slice(&record);
    }

    fn eh_placed<'a>(
        output: &'a Output,
        plan: &'a Plan,
        regions: &'a [Region],
        contents: &'a [&'a [u8]],
    ) -> Placed<'a> {
        placed(output, plan, regions, contents, &[], &[])
    }

    fn scanned(placed: &Placed<'_>) -> Result<Vec<(u64, u64, u64)>, LinkError> {
        Ok(super::fdes(placed, None)?.iter().map(|it| (it.addr, it.offset, it.end)).collect())
    }

    fn parsed(placed: &Placed<'_>) -> Result<Vec<(u64, u64, u64)>, LinkError> {
        Ok(gimli_fdes(placed)?.into_iter().map(|(addr, (at, end))| (addr, at, end)).collect())
    }

    #[test]
    fn hand_built_records_scan_as_gimli_parses_them() {
        let mut eh = Vec::new();
        let plain = eh.len();
        eh.extend_from_slice(&record(&[0, 0, 0, 0, 1, 0, 1, 0x78, 30]));
        push_fde(&mut eh, plain, &[TEXT.to_le_bytes(), 0x10u64.to_le_bytes()].concat());
        push_fde(&mut eh, plain, &[0u64.to_le_bytes(), 0x10u64.to_le_bytes()].concat());
        let udata4 = eh.len();
        let cie: [&[u8]; 3] = [&[0, 0, 0, 0, 3], b"zR\0", &[1, 0x78, 30, 1, 0x03]];
        eh.extend_from_slice(&record(&cie.concat()));
        let fde: [&[u8]; 3] = [&0x5000u32.to_le_bytes(), &0x20u32.to_le_bytes(), &[0]];
        push_fde(&mut eh, udata4, &fde.concat());
        let full = eh.len();
        let cie: [&[u8]; 3] =
            [&[0, 0, 0, 0, 1], b"zPLRS\0", &[1, 0x78, 30, 7, 0x9b, 0, 0, 0, 0, 0x1b, 0x1b]];
        eh.extend_from_slice(&record(&cie.concat()));
        let field = EH_FRAME + eh.len() as u64 + 8;
        let pc = (TEXT + 0x40).wrapping_sub(field) as u32;
        let fde: [&[u8]; 3] = [&pc.to_le_bytes(), &0x30u32.to_le_bytes(), &[4, 0, 0, 0, 0]];
        push_fde(&mut eh, full, &fde.concat());
        push_fde(&mut eh, plain, &[TEXT.to_le_bytes(), 0x80u64.to_le_bytes()].concat());

        let output = output(Arch::Aarch64);
        let plan = plan();
        let regions = [region(Class::EhFrame, EH_FRAME, eh.len() as u64)];
        let contents: [&[u8]; 1] = [&eh];
        let placed = eh_placed(&output, &plan, &regions, &contents);
        let fdes = scanned(&placed).unwrap();
        assert_eq!(fdes, parsed(&placed).unwrap());
        let starts: Vec<u64> = fdes.iter().map(|it| it.0).collect();
        assert_eq!(starts, vec![0x5000, TEXT, TEXT + 0x40], "sorted, the dead FDE left out");
        assert_eq!(fdes[1].2, TEXT + 0x10, "of two FDEs at one address the first");

        let mut bad = eh.clone();
        push_fde(&mut bad, plain + 1, &[0; 16]);
        let contents: [&[u8]; 1] = [&bad];
        let regions = [region(Class::EhFrame, EH_FRAME, bad.len() as u64)];
        let placed = eh_placed(&output, &plan, &regions, &contents);
        let error = super::fdes(&placed, None).unwrap_err();
        assert!(error.what.contains("names no CIE"), "{}", error.what);
        assert!(gimli_fdes(&placed).is_err());

        let version = record(&[0, 0, 0, 0, 2, 0, 1, 0x78, 30]);
        let contents: [&[u8]; 1] = [&version];
        let regions = [region(Class::EhFrame, EH_FRAME, version.len() as u64)];
        let placed = eh_placed(&output, &plan, &regions, &contents);
        let error = super::fdes(&placed, None).unwrap_err();
        assert!(error.what.contains("unknown version"), "{}", error.what);
        assert!(gimli_fdes(&placed).is_err());
    }

    /// A seeded image: functions with FDEs (of two CIEs, one with a personality and LSDA),
    /// compact entries (dead ones, DWARF-mode ones, duplicates, up to 3 or 4 personalities
    /// held by address or by a bind, LSDAs), both out of address order, gaps past 24 bits.
    struct Case {
        arch: Arch,
        functions: Vec<Function>,
        fdes: u64,
        eh: Vec<u8>,
        cu: Vec<u8>,
        binds: Vec<Bind>,
    }

    fn case(seed: u64) -> Case {
        let mut rng = Rng::new(seed);
        let (arch, dwarf) = match seed % 2 {
            0 => (Arch::Aarch64, ARM64_MODE_DWARF),
            _ => (Arch::X86_64, X86_64_MODE_DWARF),
        };
        let n = match seed % 8 {
            0 | 4 => 2000 + rng.below(2000),
            _ => 1 + rng.below(300),
        };
        let distinct = match seed % 8 {
            0 => 300,
            4 => 2,
            _ => [2, 30, 300][rng.below(3) as usize],
        };
        let held = if seed % 9 == 0 { 4 } else { 1 + rng.below(3) };
        let mut functions = Vec::with_capacity(n as usize);
        let mut frames: Vec<(u64, u32)> = Vec::new();
        let mut entries: Vec<(u64, u32, u32, u64, u64)> = Vec::new();
        let mut addr = TEXT;
        for i in 0..n {
            if rng.below(300) == 0 {
                addr += 0x0100_0000 + 4 * rng.below(0x400);
            }
            let size = 4 * (1 + rng.below(48));
            functions.push(function(addr, size, "f"));
            let short = |rng: &mut Rng| if rng.below(8) == 0 { size - 4 } else { size };
            let covered = rng.below(10);
            if covered < 4 || covered == 8 {
                frames.push((addr, short(&mut rng) as u32));
            }
            if (4..9).contains(&covered) {
                let encoding = if covered == 8 && rng.below(2) == 0 {
                    dwarf
                } else {
                    let k = rng.below(distinct) as u32;
                    (if k % 2 == 0 { 0x0100_0000 } else { 0x0200_0000 }) | (k << 4)
                };
                let personality = rng.below(held + 1);
                let lsda = if rng.below(4) == 0 { LSDA + 8 * i } else { 0 };
                entries.push((addr, short(&mut rng) as u32, encoding, personality, lsda));
            }
            if rng.below(40) == 0 {
                frames.push((addr, size as u32));
            }
            if rng.below(40) == 0 {
                entries.push((addr, size as u32, 0x0200_0000 | 0xff0, held, 0));
            }
            if rng.below(30) == 0 {
                entries.push((0, size as u32, 0x0100_0000, 0, LSDA));
            }
            addr += size + 4 * rng.below(3);
        }
        frames.push((addr + 0x40, 0x20));
        rng.shuffle(&mut frames);
        rng.shuffle(&mut entries);
        if seed % 7 == 3 {
            rng.shuffle(&mut functions);
        }

        let encoding =
            gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
        let pcrel = gimli::DW_EH_PE_pcrel | gimli::DW_EH_PE_sdata4;
        let mut table = FrameTable::default();
        let mut plain = CommonInformationEntry::new(encoding, 4, -8, gimli::Register(30));
        plain.fde_address_encoding = pcrel;
        let plain = table.add_cie(plain);
        let mut full = CommonInformationEntry::new(encoding, 4, -8, gimli::Register(30));
        full.fde_address_encoding = pcrel;
        full.lsda_encoding = Some(pcrel);
        full.personality = Some((
            gimli::DW_EH_PE_indirect | pcrel,
            Address::Constant(PERSONALITY.wrapping_sub(EH_FRAME)),
        ));
        let full = table.add_cie(full);
        for &(addr, len) in &frames {
            let start = Address::Constant(addr.wrapping_sub(EH_FRAME));
            let mut fde = FrameDescriptionEntry::new(start, len);
            if rng.below(3) == 0 {
                fde.lsda = Some(Address::Constant(LSDA.wrapping_sub(EH_FRAME)));
                table.add_fde(full, fde);
            } else {
                table.add_fde(plain, fde);
            }
        }
        let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
        table.write_eh_frame(&mut section).unwrap();

        let mut cu = Vec::with_capacity(entries.len() * COMPACT_ENTRY as usize);
        let mut binds = Vec::new();
        for (n, &(function, length, encoding, personality, lsda)) in entries.iter().enumerate() {
            let cell = match personality {
                k if k == held => 0,
                k if k % 2 == 0 => PERSONALITY + 0x10 * k,
                k => {
                    let addr = COMPACT + 32 * n as u64 + 16;
                    binds.push(Bind { addr, import: k as u32, addend: 0 });
                    0
                }
            };
            cu.extend_from_slice(&compact(&[(function, length, encoding, cell, lsda)]));
        }
        if seed % 3 == 0 {
            binds.reverse();
        }
        Case { arch, functions, fdes: frames.len() as u64, eh: section.0.into_vec(), cu, binds }
    }

    /// Columns of `n` functions 16 bytes apart: some covered by compact entries of a few
    /// encodings (some with LSDAs), some by FDEs, ends short of the next function or at it,
    /// long runs of plain functions whose rows the merge leaves out.
    fn merge_columns(rng: &mut Rng, n: u64) -> (Vec<Function>, Vec<super::Covered>, Vec<super::Fde>, Vec<u64>) {
        let (mut functions, mut covered, mut fdes, mut ends) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for i in 0..n {
            let addr = TEXT + 16 * i;
            functions.push(function(addr, 16, "f"));
            let end = if rng.below(4) == 0 { addr + 8 } else { addr + 16 };
            match rng.below(10) {
                0..=2 => {
                    let lsda = if rng.below(10) == 0 { LSDA + 8 * i } else { 0 };
                    let encoding = 0x0200_0000 | ((rng.below(3) as u32) << 4) | if lsda != 0 { HAS_LSDA } else { 0 };
                    covered.push(super::Covered { addr, kind: super::Kind::Compact { encoding, lsda } });
                    ends.push(end);
                }
                3..=5 => {
                    fdes.push(super::Fde { addr, end, offset: 32 * i });
                    ends.push(end);
                }
                _ => {}
            }
        }
        ends.sort_unstable();
        (functions, covered, fdes, ends)
    }

    /// The rows merged in pieces are the one merge's, at cuts amid runs the merge leaves out.
    #[test]
    fn rows_merged_in_pieces_are_the_one_merges() {
        // Sixteen threads, so the merge is cut into pieces on any machine.
        blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(16), false).unwrap().install(|| {
            for seed in [1u64, 2, 3] {
                let mut rng = Rng::new(seed);
                let n = 3 * super::MERGE as u64 + rng.below(5000);
                let (functions, covered, fdes, ends) = merge_columns(&mut rng, n);
                let output = output(Arch::Aarch64);
                let plan = plan();
                let placed = placed(&output, &plan, &[], &[], &functions, &[]);
                let got = super::rows(&placed, ARM64_MODE_DWARF, &covered, &fdes, &ends).unwrap();
                let columns = super::Columns { covered: &covered, fdes: &fdes, starts: super::Starts::Placed(&functions), ends: &ends };
                let (mut rows, mut lsdas) = (Vec::new(), Vec::new());
                let greatest = super::merge(BASE, ARM64_MODE_DWARF, columns, None, &mut rows, &mut lsdas).unwrap();
                assert!(rows.len() < functions.len(), "seed {seed}: some entries add no row");
                assert_eq!(got.rows, rows, "seed {seed}");
                assert_eq!(got.lsdas, lsdas, "seed {seed}");
                assert_eq!(got.last, greatest.max(TEXT + 16 * n), "seed {seed}");
                assert!(got.sorted);
            }
        });
    }

    /// Pages split with the shortcut for runs of DWARF-mode rows are those the walk splits.
    #[test]
    fn dwarf_runs_split_into_the_pages_the_walk_makes() {
        let mut rng = Rng::new(0x7061_6765);
        for round in 0..6 {
            let n = 2000 + rng.below(8000) as usize;
            let rows: Vec<super::Row> = (0..n as u32)
                .map(|i| {
                    let dwarf = round % 2 == 0 || rng.below(50) != 0;
                    let encoding = if dwarf { ARM64_MODE_DWARF | i } else { 0x0200_0000 | ((rng.below(4) as u32) << 4) };
                    super::Row { function: 16 * i, encoding }
                })
                .collect();
            let mut encodings = super::Encodings::new(rows.len());
            for row in rows.iter().filter(|it| it.encoding & MODE_MASK != ARM64_MODE_DWARF) {
                encodings.count(row.encoding);
            }
            let mut common = Vec::new();
            encodings.common(&mut common);
            let got = encodings.pages(&rows, common.len(), ARM64_MODE_DWARF);
            let mut walked = Vec::new();
            let mut i = 0;
            while i < rows.len() {
                let page = encodings.page(&rows, i, common.len(), ARM64_MODE_DWARF);
                i += page.count;
                walked.push(page);
            }
            let shape = |pages: &[super::Page]| pages.iter().map(|it| (it.first, it.count, it.compressed, it.locals)).collect::<Vec<_>>();
            assert_eq!(shape(&got), shape(&walked), "round {round}");
        }
    }

    /// The FDE column filled in pieces is the records' live FDEs in order.
    #[test]
    fn live_fdes_are_the_records_fdes_in_order() {
        let mut rng = Rng::new(0x6c69_7665);
        for n in [0, 1, super::CHUNK - 1, 3 * super::CHUNK + 17] {
            let records: Vec<super::Record> = (0..n as u64)
                .map(|at| match rng.below(4) {
                    0 => super::Record { at, start: u64::MAX, end: 0 },
                    1 => super::Record { at, start: 0, end: 0 },
                    _ => super::Record { at, start: 0x1000 + at, end: 0x1010 + at },
                })
                .collect();
            let want: Vec<(u64, u64, u64)> = (records.iter())
                .filter(|it| !it.cie() && it.start != 0)
                .map(|it| (it.start, it.end, it.at))
                .collect();
            let got: Vec<(u64, u64, u64)> =
                super::live_fdes(&records).iter().map(|it| (it.addr, it.end, it.offset)).collect();
            assert_eq!(got, want, "{n} records");
        }
    }

    #[test]
    fn the_fde_scan_reads_what_gimli_reads() {
        for seed in 0..16 {
            let case = case(seed);
            let output = output(case.arch);
            let plan = plan();
            let regions = [region(Class::EhFrame, EH_FRAME, case.eh.len() as u64)];
            let contents: [&[u8]; 1] = [&case.eh];
            let placed = eh_placed(&output, &plan, &regions, &contents);
            assert_eq!(scanned(&placed).unwrap(), parsed(&placed).unwrap(), "seed {seed}");
        }
    }

    #[test]
    fn the_new_tables_answer_as_the_regular_page_builder_did() {
        let (mut compressed, mut regular_pages, mut errors) = (0, 0, 0);
        for seed in 0..48 {
            let case = case(seed);
            let output = output(case.arch);
            let plan = plan();
            let regions = [
                region(Class::EhFrame, EH_FRAME, case.eh.len() as u64),
                compact_region(&case.cu),
            ];
            let contents: [&[u8]; 2] = [&case.eh, &case.cu];
            let placed = placed(&output, &plan, &regions, &contents, &case.functions, &case.binds);
            let new = build(&placed, case.arch).map_err(|it| it.what);
            let old = regular(&placed, case.arch).map_err(|it| it.what);
            let (new, old) = match (new, old) {
                (Ok(new), Ok(old)) => (new, old),
                (Err(new), Err(old)) => {
                    assert_eq!(new, old, "seed {seed}: the same error");
                    errors += 1;
                    continue;
                }
                (new, old) => panic!("seed {seed}: {:?} where it was {:?}", new.err(), old.err()),
            };
            assert_eq!(build(&placed, case.arch).unwrap(), new, "seed {seed}: deterministic");
            let limit = bound(case.functions.len() as u64, case.fdes, case.cu.len() as u64);
            assert!(new.len() as u64 <= limit, "seed {seed}: {} past {limit}", new.len());
            assert!(new.len() <= old.len() + 4, "seed {seed}: {} against {}", new.len(), old.len());

            let (new, old) = (Table { bytes: &new }, Table { bytes: &old });
            let entries = old.entries();
            assert_eq!(new.entries(), entries, "seed {seed}: the same entries");
            assert_eq!(new.personalities(), old.personalities(), "seed {seed}");
            assert_eq!(new.lsdas(), old.lsdas(), "seed {seed}: the same LSDA index");
            let sentinel = old.index().last().unwrap().0;
            assert_eq!(new.index().last().unwrap().0, sentinel, "seed {seed}: the sentinel");
            let boundaries = entries.iter().map(|it| it.0).chain([sentinel]);
            for start in boundaries {
                for pc in [start.wrapping_sub(1), start, start + 1, start + 2] {
                    assert_eq!(new.find(pc), old.find(pc), "seed {seed}: pc {pc:#x}");
                }
            }
            assert_eq!(new.find(0), old.find(0), "seed {seed}: before every function");
            for kind in new.kinds() {
                match kind {
                    3 => compressed += 1,
                    _ => regular_pages += 1,
                }
            }
        }
        assert!(compressed > 0, "some pages compressed");
        assert!(regular_pages > 0, "some pages regular, where a compressed one held fewer");
        assert!(errors < 48, "most seeds build");
    }

    /// The FDEs as the builder this one replaced read them: gimli's full parse, a map by start.
    fn gimli_fdes(placed: &Placed<'_>) -> Result<BTreeMap<u64, (u64, u64)>, LinkError> {
        let mut fdes = BTreeMap::new();
        let Some((index, region)) = placed.region(&Class::EhFrame) else {
            return Ok(fdes);
        };
        let bytes = flat(region_bytes(placed, index, region, "__eh_frame")?);
        let mut section = gimli::read::EhFrame::new(bytes, gimli::LittleEndian);
        section.set_address_size(8);
        let bases = BaseAddresses::default().set_eh_frame(region.addr);
        let bad = |at: String, err: gimli::Error| {
            error(format!("__eh_frame at {:#x}: {at}: {err}", region.addr))
        };
        let mut entries = section.entries(&bases);
        while let Some(entry) = entries.next().map_err(|err| bad("reading an entry".into(), err))? {
            let CieOrFde::Fde(partial) = entry else {
                continue;
            };
            let at = partial.offset() as u64;
            let fde = partial
                .parse(|section, bases, offset| section.cie_from_offset(bases, offset))
                .map_err(|err| bad(format!("the FDE at offset {at:#x}"), err))?;
            let start = fde.initial_address();
            if start == 0 {
                continue;
            }
            let end = start.checked_add(fde.len()).ok_or_else(|| {
                error(format!("__eh_frame at {:#x}: the FDE at {at:#x} overflows", region.addr))
            })?;
            fdes.entry(start).or_insert((at, end));
        }
        Ok(fdes)
    }

    enum OldKind {
        Compact { encoding: u32, lsda: Option<u64> },
        Dwarf { fde: u64 },
        None,
    }

    struct OldEntry {
        addr: u64,
        rank: u8,
        kind: OldKind,
    }

    struct OldRow {
        function: u32,
        encoding: u32,
        lsda: Option<u32>,
    }

    /// The builder this one replaced, as it was: gimli's FDE map, one sort of every start,
    /// regular pages only. The differential test holds `build` to its answers.
    fn regular(placed: &Placed<'_>, arch: Arch) -> Result<Vec<u8>, LinkError> {
        const PAGE_ENTRIES: usize = 511;
        let dwarf_mode = match arch {
            Arch::Aarch64 => ARM64_MODE_DWARF,
            Arch::X86_64 => X86_64_MODE_DWARF,
            Arch::Wasm32 => return Err(error("wasm32".into())),
        };
        let base = placed.plan.base;
        let fdes = gimli_fdes(placed)?;

        let mut entries = Vec::new();
        let mut ends = Vec::new();
        let mut personalities: Vec<(Held, u32)> = Vec::new();
        let class = super::super::compact_unwind();
        let regions = placed.regions.iter().enumerate().filter(|(_, it)| it.class == class);
        for (index, region) in regions {
            let bytes = flat(region_bytes(placed, index, region, "__compact_unwind")?);
            let binds: BTreeMap<u64, Held> = placed
                .binds
                .iter()
                .filter(|it| (region.addr..region.addr + region.size).contains(&it.addr))
                .map(|it| (it.addr, Held::Import { import: it.import, addend: it.addend }))
                .collect();
            for (n, entry) in bytes.chunks_exact(COMPACT_ENTRY as usize).enumerate() {
                let function = u64_at(entry, 0);
                if function == 0 {
                    continue;
                }
                let end = function + u64::from(u32_at(entry, 8));
                let encoding = u32_at(entry, 12);
                let personality = u64_at(entry, 16);
                let lsda = u64_at(entry, 24);
                let kind = if encoding & MODE_MASK == dwarf_mode {
                    let Some(&(fde, _)) = fdes.get(&function) else {
                        return Err(error(format!(
                            "__compact_unwind: the entry of {} says DWARF mode ({encoding:#010x}), \
                             but no FDE in __eh_frame starts there",
                            named(placed, function)
                        )));
                    };
                    OldKind::Dwarf { fde }
                } else {
                    let mut encoding = encoding & !(PERSONALITY_MASK | HAS_LSDA);
                    let cell = region.addr + COMPACT_ENTRY * n as u64 + 16;
                    let held = match binds.get(&cell) {
                        Some(bind) => Some(*bind),
                        None => (personality != 0).then_some(Held::Address(personality)),
                    };
                    if let Some(held) = held {
                        let index = match personalities.iter().position(|(it, _)| *it == held) {
                            Some(index) => index,
                            None => {
                                if personalities.len() == PERSONALITY_MAX {
                                    return Err(error(format!(
                                        "__unwind_info: {} has a fourth distinct personality \
                                         (held at {cell:#x}); the format has room for \
                                         {PERSONALITY_MAX}",
                                        named(placed, function)
                                    )));
                                }
                                let cell = offset(base, cell, "a personality's cell")?;
                                personalities.push((held, cell));
                                personalities.len() - 1
                            }
                        };
                        encoding |= ((index as u32) + 1) << PERSONALITY_SHIFT;
                    }
                    let lsda = (lsda != 0).then_some(lsda);
                    if lsda.is_some() {
                        encoding |= HAS_LSDA;
                    }
                    OldKind::Compact { encoding, lsda }
                };
                entries.push(OldEntry { addr: function, rank: 0, kind });
                ends.push(end);
            }
        }
        for (&start, &(fde, end)) in &fdes {
            entries.push(OldEntry { addr: start, rank: 1, kind: OldKind::Dwarf { fde } });
            ends.push(end);
        }
        let mut last = base;
        for function in placed.functions {
            entries.push(OldEntry { addr: function.addr, rank: 2, kind: OldKind::None });
            last = last.max(function.addr.saturating_add(function.size));
        }
        for &end in &ends {
            entries.push(OldEntry { addr: end, rank: 3, kind: OldKind::None });
            last = last.max(end);
        }
        entries.sort_by_key(|it| (it.addr, it.rank));
        entries.dedup_by_key(|it| it.addr);
        if let Some(entry) = entries.last() {
            last = last.max(entry.addr);
        }

        let mut rows: Vec<OldRow> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let (encoding, lsda) = match entry.kind {
                OldKind::Compact { encoding, lsda } => (encoding, lsda),
                OldKind::Dwarf { fde } => (super::dwarf_encoding(dwarf_mode, fde), None),
                OldKind::None => (0, None),
            };
            if let Some(previous) = rows.last()
                && previous.encoding == encoding
                && previous.lsda.is_none()
                && lsda.is_none()
            {
                continue;
            }
            let lsda = match lsda {
                Some(lsda) => Some(offset(base, lsda, "an LSDA")?),
                None => None,
            };
            rows.push(OldRow { function: offset(base, entry.addr, "a function")?, encoding, lsda });
        }
        let sentinel = offset(base, last, "the end of the last function")?;

        let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
        for row in &rows {
            *counts.entry(row.encoding).or_default() += 1;
        }
        let mut common: Vec<(u32, u64)> = counts.into_iter().collect();
        common.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        common.truncate(COMMON_MAX);

        let pages: Vec<&[OldRow]> = rows.chunks(PAGE_ENTRIES).collect();
        let lsdas: Vec<(u32, u32)> =
            rows.iter().filter_map(|row| row.lsda.map(|lsda| (row.function, lsda))).collect();
        let common_at = HEADER;
        let personalities_at = common_at + 4 * common.len() as u64;
        let index_at = personalities_at + 4 * personalities.len() as u64;
        let lsda_at = index_at + INDEX_ENTRY * (pages.len() as u64 + 1);
        let pages_at = lsda_at + LSDA_ENTRY * lsdas.len() as u64;
        let small = |value: u64| value as u32;

        let mut out = Vec::new();
        for value in [
            VERSION,
            small(common_at),
            common.len() as u32,
            small(personalities_at),
            personalities.len() as u32,
            small(index_at),
            pages.len() as u32 + 1,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for (encoding, _) in &common {
            out.extend_from_slice(&encoding.to_le_bytes());
        }
        for (_, cell) in &personalities {
            out.extend_from_slice(&cell.to_le_bytes());
        }
        let mut page_at = pages_at;
        let mut lsda_index = 0usize;
        for page in &pages {
            let first = page[0].function;
            while lsdas.get(lsda_index).is_some_and(|it| it.0 < first) {
                lsda_index += 1;
            }
            for value in [first, small(page_at), small(lsda_at + LSDA_ENTRY * lsda_index as u64)] {
                out.extend_from_slice(&value.to_le_bytes());
            }
            page_at += PAGE_HEADER + PAGE_ENTRY * page.len() as u64;
        }
        for value in [sentinel, 0, small(lsda_at + LSDA_ENTRY * lsdas.len() as u64)] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for (function, lsda) in &lsdas {
            out.extend_from_slice(&function.to_le_bytes());
            out.extend_from_slice(&lsda.to_le_bytes());
        }
        for page in &pages {
            out.extend_from_slice(&SECOND_LEVEL_REGULAR.to_le_bytes());
            out.extend_from_slice(&(PAGE_HEADER as u16).to_le_bytes());
            out.extend_from_slice(&(page.len() as u16).to_le_bytes());
            for row in *page {
                out.extend_from_slice(&row.function.to_le_bytes());
                out.extend_from_slice(&row.encoding.to_le_bytes());
            }
        }
        Ok(out)
    }

    fn placed<'a>(
        output: &'a Output,
        plan: &'a Plan,
        regions: &'a [Region],
        contents: &'a [&'a [u8]],
        functions: &'a [Function],
        binds: &'a [Bind],
    ) -> Placed<'a> {
        Placed {
            output,
            name: "test",
            plan,
            regions,
            contents,
            paged: &[],
            libraries: &[],
            imports: &[],
            got: &[],
            rebase: &[],
            binds,
            exports: &[],
            functions,
            entry: None,
            init: None,
            fini: None,
            tls: None,
            id: [0; 16],
            debug: &[],
            symbol: &no_symbol,
            synthesis: 0,
            delta: None,
            encoded: &[],
        }
    }
}
