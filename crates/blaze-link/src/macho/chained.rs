//! Chained fixups (`LC_DYLD_CHAINED_FIXUPS`), as ld64 and ld-prime write them for a deployment
//! target that has them: every slot dyld slides or binds holds, in the file, its target and the
//! distance to the next slot of its 16 KiB page, `DYLD_CHAINED_PTR_64_OFFSET` (format 6). The
//! regions keep the plain values; [`encode`] gives the slots ([`crate::model::Writer::encode`])
//! and [`fixups`] the table dyld reads to find each page's first slot and each bind's import.
//!
//! On an incremental link a changed slot rewrites its 8 bytes and its predecessor's `next`,
//! where an opcode stream shifts after the change.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use blaze_format::cursor::Put;
use blaze_ids::radix;
use object::macho;
use smallvec::SmallVec;

use super::{Segment, error, place};
use crate::LinkError;
use crate::model::{Holds, Placed};
use crate::output::Platform;

/// The page each chain runs in.
pub(super) const PAGE: u64 = 0x4000;
/// `DYLD_CHAINED_PTR_64_OFFSET`.
const POINTER_FORMAT: u16 = 6;
/// `DYLD_CHAINED_IMPORT` and `DYLD_CHAINED_IMPORT_ADDEND`.
const IMPORT: u32 = 1;
const IMPORT_ADDEND: u32 = 2;
/// `DYLD_CHAINED_PTR_START_NONE`: a page with no slot.
const START_NONE: u16 = 0xffff;
const HEADER: usize = 28;
const TARGET_BITS: u32 = 36;
const NEXT_SHIFT: u32 = 51;
const BIND: u64 = 1 << 63;
const ORDINAL_BITS: u32 = 24;
const NAME_BITS: u32 = 23;

/// Whether an image for `platform` is fixed up by chains: ld-prime's choice for the platforms
/// it knows (macOS 12, iOS, tvOS and Mac Catalyst 15, watchOS 8, DriverKit 21, every visionOS).
/// Other platforms keep the rebase and bind opcodes, which every dyld reads.
pub(super) fn chained(platform: &Platform) -> bool {
    let from = match macho::Platform(platform.platform) {
        macho::PLATFORM_MACOS => 0x000c_0000,
        macho::PLATFORM_IOS
        | macho::PLATFORM_IOSSIMULATOR
        | macho::PLATFORM_TVOS
        | macho::PLATFORM_TVOSSIMULATOR
        | macho::PLATFORM_MACCATALYST => 0x000f_0000,
        macho::PLATFORM_WATCHOS | macho::PLATFORM_WATCHOSSIMULATOR => 0x0008_0000,
        macho::PLATFORM_DRIVERKIT => 0x0015_0000,
        macho::PLATFORM_XROS | macho::PLATFORM_XROSSIMULATOR => 0,
        _ => return false,
    };
    platform.minimum >= from
}

/// The segments dyld writes (`__DATA_CONST`, `__DATA`) as [`super::MachO::image`] lays them:
/// from their first region's address to the page after their last region's room.
pub(super) fn writable(placed: &Placed<'_>, page: u64) -> SmallVec<[(u64, u64); 2]> {
    let mut out: SmallVec<[(u64, u64); 2]> = SmallVec::new();
    for segment in [Segment::DataConst, Segment::Data] {
        let mut regions = placed.regions.iter().filter(|it| place(&it.class).0 == segment);
        let Some(first) = regions.next() else { continue };
        let end = regions.fold(first.addr + first.capacity, |end, it| end.max(it.addr + it.capacity));
        out.push((first.addr, end.next_multiple_of(page)));
    }
    out
}

/// The import table: by entry, an import and the addend its binds add.
pub(super) struct Imports {
    format: u32,
    /// Past the imports themselves (entry `n` is import `n`, addend 0), each (import, addend)
    /// a bind adds that its pointer can't hold, sorted: the `_ADDEND` form's.
    extra: Vec<(u32, i32)>,
    count: u32,
}

/// Every bind, (address, import, addend): `placed.binds` and the GOT's import cells, by address.
fn binds(placed: &Placed<'_>) -> Result<Vec<(u64, u32, i64)>, LinkError> {
    let mut out = Vec::with_capacity(placed.binds.len() + placed.got.len());
    out.extend(placed.binds.iter().map(|it| (it.addr, it.import, it.addend)));
    for cell in placed.got {
        match cell.holds {
            Holds::Local(_) => {}
            Holds::Import(import) => out.push((cell.addr, import, 0)),
            ref other => {
                // alloc: error path
                return Err(error(format!(
                    "a GOT cell at {:#x} holding {other:?}, which is ELF's and not Mach-O's",
                    cell.addr
                )));
            }
        }
    }
    if !out.is_sorted_by_key(|it| it.0) {
        out.sort_unstable_by_key(|it| it.0);
    }
    Ok(out)
}

fn fits_pointer(addend: i64) -> bool {
    i8::try_from(addend).is_ok()
}

impl Imports {
    fn of(placed: &Placed<'_>, binds: &[(u64, u32, i64)]) -> Result<Imports, LinkError> {
        let count = u32::try_from(placed.imports.len())
            .ok()
            .filter(|it| *it < 1 << ORDINAL_BITS)
            // alloc: error path
            .ok_or_else(|| error(format!("{} imports: a chained bind names fewer than 2^24", placed.imports.len())))?;
        if let Some(&(addr, import, _)) = binds.iter().find(|it| it.1 >= count) {
            // alloc: error path
            return Err(error(format!("a bind at {addr:#x} of no import ({import})")));
        }
        if binds.iter().all(|it| fits_pointer(it.2)) {
            // alloc: none: an empty `Vec` holds no buffer
            return Ok(Imports { format: IMPORT, extra: Vec::new(), count });
        }
        let mut extra = Vec::with_capacity(binds.len());
        for &(addr, import, addend) in binds.iter().filter(|it| it.2 != 0) {
            let addend = i32::try_from(addend)
                // alloc: error path
                .map_err(|_| error(format!("a bind at {addr:#x} adds {addend}, past what a chained import holds")))?;
            extra.push((import, addend));
        }
        radix::sort_copy_by_key(&mut extra, |&(import, addend)| radix::pack_32(import, radix::signed_32(addend)));
        extra.dedup();
        let total = count as usize + extra.len();
        if total >= 1 << ORDINAL_BITS {
            // alloc: error path
            return Err(error(format!("{total} chained imports, more than a bind names")));
        }
        Ok(Imports { format: IMPORT_ADDEND, extra, count })
    }

    /// The entry a bind of `import` adding `addend` names, and the addend its pointer holds.
    fn entry(&self, import: u32, addend: i64) -> (u32, i64) {
        match self.format {
            IMPORT => (import, addend),
            _ if addend == 0 => (import, 0),
            _ => {
                let key = (import, addend as i32);
                let at = self.extra.binary_search(&key).expect("every addend a bind adds has its entry");
                (self.count + at as u32, 0)
            }
        }
    }

    fn entries(&self) -> impl Iterator<Item = (u32, i32)> + '_ {
        (0..self.count).map(|it| (it, 0)).chain(self.extra.iter().copied())
    }
}

/// The 8 bytes at `addr` of the placed image.
fn value_at(placed: &Placed<'_>, addr: u64) -> Result<u64, LinkError> {
    placed
        .u64_at(addr)
        // alloc: error path
        .ok_or_else(|| error(format!("a pointer to slide at {addr:#x}, outside the bytes of its region")))
}

/// The segment of `segments` holding `addr`, by its start; an error when none does.
fn segment_of(segments: &[(u64, u64)], addr: u64, what: &str) -> Result<u64, LinkError> {
    (segments.iter())
        .find(|it| (it.0..it.1).contains(&addr))
        .map(|it| it.0)
        // alloc: error path
        .ok_or_else(|| error(format!("{what} at {addr:#x} is in no segment dyld writes")))
}

/// Every slot dyld slides or binds, (address, the 8 bytes the file holds), in address order.
pub(super) fn encode(placed: &Placed<'_>, page: u64) -> Result<Vec<(u64, u64)>, LinkError> {
    let base = placed.plan.base;
    let segments = writable(placed, page);
    let binds = binds(placed)?;
    let imports = Imports::of(placed, &binds)?;
    let rebase: Cow<'_, [u64]> = if placed.rebase.is_sorted() {
        Cow::Borrowed(placed.rebase)
    } else {
        // alloc: only for addresses out of order, which `Placed` lists in order
        let mut it = placed.rebase.to_vec();
        radix::sort(&mut it);
        Cow::Owned(it)
    };
    let mut slots: Vec<(u64, u64)> = Vec::with_capacity(rebase.len() + binds.len());
    let (mut r, mut b) = (0, 0);
    while r < rebase.len() || b < binds.len() {
        let take_rebase = match (rebase.get(r), binds.get(b)) {
            (Some(&at), Some(bind)) if at == bind.0 => {
                // alloc: error path
                return Err(error(format!("the address {at:#x} is both slid and bound")));
            }
            (Some(&at), Some(bind)) => at < bind.0,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if take_rebase {
            let at = rebase[r];
            r += 1;
            let value = value_at(placed, at)?;
            let target = value.checked_sub(base).filter(|it| *it < 1 << TARGET_BITS).ok_or_else(|| {
                // alloc: error path
                error(format!("the pointer at {at:#x} holds {value:#x}, which a chained rebase from {base:#x} can't"))
            })?;
            slots.push((at, target));
        } else {
            let (at, import, addend) = binds[b];
            b += 1;
            let (entry, addend) = imports.entry(import, addend);
            slots.push((at, BIND | u64::from(entry) | (u64::from(addend as i8 as u8) << ORDINAL_BITS)));
        }
    }
    for nth in 0..slots.len() {
        let at = slots[nth].0;
        if at % 8 != 0 {
            // alloc: error path
            return Err(error(format!("a pointer dyld fixes up at {at:#x}, not 8-aligned")));
        }
        if slots.get(nth + 1).is_some_and(|it| it.0 < at + 8) {
            // alloc: error path
            return Err(error(format!("the pointers dyld fixes up at {at:#x} overlap")));
        }
        let start = segment_of(&segments, at, "a pointer dyld fixes up")?;
        let page_end = at + (PAGE - (at - start) % PAGE);
        let next = slots.get(nth + 1).filter(|it| it.0 < page_end).map_or(0, |it| (it.0 - at) / 4);
        slots[nth].1 |= next << NEXT_SHIFT;
    }
    Ok(slots)
}

/// How many slots [`encode`] gives for `placed`: what [`fixups`] checks `Placed::encoded` by.
fn slots_of(placed: &Placed<'_>) -> usize {
    placed.rebase.len()
        + placed.binds.len()
        + placed.got.iter().filter(|it| matches!(it.holds, Holds::Import(_))).count()
}

/// Appends `LC_DYLD_CHAINED_FIXUPS`'s table: the header, the starts of each segment's pages
/// (`segments` the image's `LC_SEGMENT_64`s before `__LINKEDIT`, (start, size in the file), and
/// `__LINKEDIT` after them), the imports and their names. `slots` are [`encode`]'s,
/// `ordinal` an import's library ordinal and `name` its Mach-O name. Padded to 8; its length.
pub(super) fn fixups<'n>(
    placed: &Placed<'_>,
    slots: &[(u64, u64)],
    segments: &[(u64, u64)],
    ordinal: impl Fn(u32) -> Result<i64, LinkError>,
    name: impl Fn(u32) -> &'n [u8],
    out: &mut Vec<u8>,
) -> Result<usize, LinkError> {
    let base = placed.plan.base;
    if slots.len() != slots_of(placed) {
        // alloc: error path
        return Err(error(format!(
            "{} chained slots for {} pointers dyld fixes up: `Placed::encoded` is not this image's",
            slots.len(),
            slots_of(placed)
        )));
    }
    let binds = binds(placed)?;
    let imports = Imports::of(placed, &binds)?;
    let start = out.len();
    out.resize(start + HEADER.next_multiple_of(8), 0);

    // `dyld_chained_starts_in_image`, then each segment's `dyld_chained_starts_in_segment`.
    let starts = out.len();
    let count = segments.len() + 1;
    out.put_u32(count as u32);
    let offsets = out.len();
    out.resize(offsets + 4 * count, 0);
    let mut next = 0;
    for (nth, &(vmaddr, size)) in segments.iter().enumerate() {
        let first = next + slots[next..].partition_point(|it| it.0 < vmaddr);
        if first != next {
            // alloc: error path
            return Err(error(format!("a chained slot at {:#x} is in no segment's bytes in the file", slots[next].0)));
        }
        let end = first + slots[first..].partition_point(|it| it.0 < vmaddr + size);
        next = end;
        if first == end {
            continue;
        }
        let pad = (out.len() - start).next_multiple_of(8) - (out.len() - start);
        out.resize(out.len() + pad, 0);
        let at = (out.len() - starts) as u32;
        out.patch_u32(offsets + 4 * nth, at);
        let pages = size.div_ceil(PAGE) as usize;
        if pages > usize::from(u16::MAX) {
            // alloc: error path
            return Err(error(format!("a segment at {vmaddr:#x} of {pages} chained pages, more than 65,535")));
        }
        out.put_u32((22 + 2 * pages) as u32);
        out.put_u16(PAGE as u16);
        out.put_u16(POINTER_FORMAT);
        out.put_u64(vmaddr - base);
        out.put_u32(0);
        out.put_u16(pages as u16);
        let page_starts = out.len();
        out.resize(page_starts + 2 * pages, 0);
        for page in out[page_starts..].chunks_exact_mut(2) {
            page.copy_from_slice(&START_NONE.to_le_bytes());
        }
        let mut last = usize::MAX;
        for &(addr, _) in &slots[first..end] {
            let page = ((addr - vmaddr) / PAGE) as usize;
            if page != last {
                let offset = ((addr - vmaddr) % PAGE) as u16;
                out[page_starts + 2 * page..page_starts + 2 * page + 2].copy_from_slice(&offset.to_le_bytes());
                last = page;
            }
        }
    }
    if next != slots.len() {
        // alloc: error path
        return Err(error(format!("a chained slot at {:#x} is in no segment", slots[next].0)));
    }

    // The imports, then their names, each once.
    let pad = (out.len() - start).next_multiple_of(4) - (out.len() - start);
    out.resize(out.len() + pad, 0);
    let imports_at = out.len() - start;
    let entries = imports.count as usize + imports.extra.len();
    let entry_len = if imports.format == IMPORT { 4 } else { 8 };
    let table = out.len();
    out.resize(table + entries * entry_len, 0);
    let symbols_at = out.len() - start;
    // By import: its name's offset among the symbols.
    // alloc: one table per link, sized once by the imports
    let mut named = vec![u32::MAX; imports.count as usize];
    for (nth, (import, addend)) in imports.entries().enumerate() {
        let offset = match named[import as usize] {
            u32::MAX => {
                let offset = (out.len() - start - symbols_at) as u32;
                if offset >= 1 << NAME_BITS {
                    // alloc: error path
                    return Err(error("the chained imports' names pass 8 MiB, which their offsets reach"));
                }
                out.extend_from_slice(name(import));
                out.push(0);
                named[import as usize] = offset;
                offset
            }
            offset => offset,
        };
        let weak = placed.imports[import as usize].weak;
        let word = u32::from(ordinal(import)? as u8) | u32::from(weak) << 8 | offset << 9;
        let at = table + nth * entry_len;
        out.patch_u32(at, word);
        if entry_len == 8 {
            out.patch_u32(at + 4, addend as u32);
        }
    }

    // `dyld_chained_fixups_header`.
    let header = [0, (starts - start) as u32, imports_at as u32, symbols_at as u32, entries as u32, imports.format, 0];
    for (nth, field) in header.into_iter().enumerate() {
        out.patch_u32(start + 4 * nth, field);
    }
    let len = (out.len() - start).next_multiple_of(8);
    out.resize(start + len, 0);
    Ok(len)
}
