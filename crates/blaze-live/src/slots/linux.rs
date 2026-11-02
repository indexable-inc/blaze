//! Writing a running ELF image's data on Linux (x86-64 and AArch64).
//!
//! The patch code region is the daemon's code memory (blaze-inproc), mapped by the start hook
//! before `main`: the commit writes the code there, and nothing here maps over the region or
//! writes it. The image's mappings and their protections are read from /proc/self/maps once,
//! at start. Read-only pages are written through a second mapping: the first write into one
//! moves a window of its mapping ([`WINDOW`], within the mapping) onto a memory file mapped
//! twice, with the protection it had ([`Aliases`]), so that write and every later one into the
//! window are stores through the second mapping, with no system call and no TLB shootdown:
//! what JITs that keep code W^X by mapping it twice do. The table cell's page, when read-only,
//! gets its window at start. Data is written with atomic stores ([`Writes::store`]), since
//! other threads may read it meanwhile.

#![allow(
    unsafe_code,
    reason = "the image's protections, its read-only pages' windows and unwind registration"
)]

use std::{
    ffi::{c_char, c_void},
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) use super::unix::{GRAIN, Reserved, backtrace, descriptor, in_image_of};
use super::{
    switch::{Dwarf, Layout, Memory, Span, Writes},
    unix::{__register_frame, Segments, load_cell, loaders, mmap, munmap},
};

unsafe extern "C" {
    fn mremap(old: *mut c_void, old_len: usize, new_len: usize, flags: i32, ...) -> *mut c_void;
    #[cfg(test)]
    fn sysconf(name: i32) -> i64;
    fn memfd_create(name: *const c_char, flags: u32) -> i32;
    fn ftruncate(fd: i32, len: i64) -> i32;
    fn close(fd: i32) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_SHARED: i32 = 1;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
const MREMAP_MAYMOVE: i32 = 1;
const MREMAP_FIXED: i32 = 2;
#[cfg(test)]
const SC_PAGESIZE: i32 = 30;
const MFD_CLOEXEC: u32 = 1;
/// What a window of read-only pages written through a second mapping is aligned to and at
/// most as long as: the first write into it copies at most this much (about 0.2 ms at
/// 10 GB/s, once), and a file-backed page in it costs a page of memory from then on.
const WINDOW: u64 = 2 << 20;

pub(super) struct Os {
    /// The protection of the table cell's page.
    cell: i32,
    maps: Maps,
    /// Where the image is mapped, which every data write is checked to be in.
    segments: Segments,
    aliases: Aliases,
}

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        let maps = Maps::read()?;
        let code = layout.code_in_memory();
        if !code.is_empty() {
            maps.each(&code, readable)?;
        }
        let at = layout.cell();
        let segments = Segments::of(at)?;
        let (mapping, cell) = maps.mapping(at)?;
        let mut aliases = Aliases::new();
        if !code.is_empty() {
            // No window of read-only pages is cut over the code memory's mapping: data is
            // never written into it (the switch refuses that), and the window's address, 0,
            // is never written through.
            aliases.windows.push((code.start, code.end, 0));
        }
        if cell & PROT_WRITE == 0 {
            if cell & PROT_READ == 0 {
                // alloc: error path, at start
                return Err(format!("the table cell at {at:#x} is in pages with no access"));
            }
            segments.check(&(at..at + 8))?;
            aliases.make(at, &mapping, cell)?;
        }
        Ok(Os { cell, maps, segments, aliases })
    }
}

/// Read-only pages of the image that patches write, each window of them moved onto a memory
/// file mapped twice ([`map_twice`]): where the image has it, with the protection it had, and
/// read and write at an address of its own. Sorted by address, none overlapping, each within
/// one mapping of the image. A window is made once, at the first write into it, and kept for
/// the life of the program. The patch code region is the first, put there at start, with no
/// second mapping: no window is cut over the code memory.
struct Aliases {
    /// Start and end in memory, and where the window is mapped writable.
    windows: Vec<(u64, u64, u64)>,
}

impl Aliases {
    fn new() -> Aliases {
        // alloc: setup, once at start; an edit writes one or two windows, and a window is
        // added at most once per `WINDOW` of read-only image the program's patches ever write
        Aliases { windows: Vec::with_capacity(16) }
    }

    /// The window that has `at`: a binary search.
    fn find(&self, at: u64) -> Option<usize> {
        let next = self.windows.partition_point(|it| it.1 <= at);
        self.windows.get(next).filter(|it| it.0 <= at).map(|_| next)
    }

    /// Makes the window that `at`, in `mapping` (whole pages, read-only with `protection`),
    /// falls in, and has no window yet: its index.
    fn make(&mut self, at: u64, mapping: &Range<u64>, protection: i32) -> Result<usize, String> {
        let next = self.windows.partition_point(|it| it.1 <= at);
        let low = next.checked_sub(1).map_or(0, |it| self.windows[it].1);
        let high = self.windows.get(next).map_or(u64::MAX, |it| it.0);
        let window = window(at, mapping, low..high);
        let alias = map_twice(&window, protection)?;
        // alloc: once per window, the first write into it (see `Aliases::new`)
        self.windows.insert(next, (window.start, window.end, alias));
        Ok(next)
    }

    /// The address `at` is written at, in the window [`Aliases::make`] made or makes for
    /// it, and where that window ends.
    fn writable(
        &mut self,
        at: u64,
        mapping: &Range<u64>,
        protection: i32,
    ) -> Result<(u64, u64), String> {
        let index = match self.find(at) {
            Some(index) => index,
            None => self.make(at, mapping, protection)?,
        };
        let (start, end, alias) = self.windows[index];
        Ok((alias + (at - start), end))
    }

    /// Writes what `writes` has within `piece`, read-only pages of `mapping` (one mapping
    /// of the image, with `protection`), through the windows' second mappings.
    fn store(
        &mut self,
        writes: &Writes<'_>,
        piece: &Range<u64>,
        mapping: &Range<u64>,
        protection: i32,
    ) -> Result<(), String> {
        let mut at = piece.start;
        while at < piece.end {
            let (to, end) = self.writable(at, mapping, protection)?;
            let part = at..end.min(piece.end);
            // SAFETY: `part` is in one window, which `to` maps writable at the same offsets
            // for the life of the program; the pages are the image's (the caller checked),
            // and other threads may read them meanwhile, which `store` allows.
            unsafe { writes.store(&part, to as *mut u8) };
            if protection & PROT_EXEC != 0 {
                // clone: a Range<u64> of two words; Range isn't Copy
                flush(std::iter::once(part.clone()), to.wrapping_sub(part.start));
            }
            at = part.end;
        }
        Ok(())
    }
}

/// The window of read-only pages that `at` falls in: [`WINDOW`]-aligned, within `mapping`
/// (whole pages) and within `free`, the room between the windows made before it (`at` is in
/// all three). Whole pages, since each of its bounds is.
fn window(at: u64, mapping: &Range<u64>, free: Range<u64>) -> Range<u64> {
    let start = (at & !(WINDOW - 1)).max(mapping.start).max(free.start);
    let end = (at | (WINDOW - 1)).saturating_add(1).min(mapping.end).min(free.end);
    start..end
}

// Not blaze-inproc's `View`: the pages it maps over are the image's, which threads may be
// reading.
/// Puts a memory file under `range` (whole pages, mapped readable), with the bytes it has,
/// mapped there with `protection`; the address it is mapped read and write at.
fn map_twice(range: &Range<u64>, protection: i32) -> Result<u64, String> {
    let code = range;
    let len = (code.end - code.start) as usize;
    let error = |what: &str| {
        // alloc: error path
        format!(
            "{what} for {:#x}..{:#x}: {}",
            code.start,
            code.end,
            std::io::Error::last_os_error()
        )
    };
    // SAFETY: the name ends in a zero.
    let fd = unsafe { memfd_create(c"blaze-live-code".as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(error("making a memory file"));
    }
    let mapped = (|| {
        // SAFETY: `fd` is this function's file.
        if unsafe { ftruncate(fd, len as i64) } != 0 {
            return Err(error("sizing a memory file"));
        }
        // SAFETY: a new shared mapping of the file, anywhere.
        let alias =
            unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
        if alias == MAP_FAILED {
            return Err(error("mapping a memory file"));
        }
        // SAFETY: `code` is mapped and readable (the caller checked), `alias` is `len` bytes
        // of new memory; they don't overlap.
        unsafe { std::ptr::copy_nonoverlapping(code.start as *const u8, alias as *mut u8, len) };
        // SAFETY: as for `alias`.
        let view = unsafe { mmap(std::ptr::null_mut(), len, protection, MAP_SHARED, fd, 0) };
        if view == MAP_FAILED {
            let error = error("mapping a memory file with the protection it replaces");
            // SAFETY: this function's mapping, which nothing refers to.
            unsafe { munmap(alias, len) };
            return Err(error);
        }
        let to = code.start as *mut c_void;
        // SAFETY: `view` is this function's mapping of the bytes the region has, with its
        // protection; moving it over the region replaces it in one step, so a thread reading
        // or running it sees the same bytes before and after. Nothing writes the region
        // meanwhile: it is read-only pages of the image.
        let moved = unsafe { mremap(view, len, len, MREMAP_MAYMOVE | MREMAP_FIXED, to) };
        if moved != to {
            let error = error("moving a memory file over the region");
            // SAFETY: this function's mapping, which nothing refers to.
            unsafe { munmap(view, len) };
            // SAFETY: as above.
            unsafe { munmap(alias, len) };
            return Err(error);
        }
        Ok(alias as u64)
    })();
    // SAFETY: the mappings keep the file; its descriptor is this function's.
    unsafe { close(fd) };
    mapped
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Sections;
    const SEH: bool = false;

    fn cell(&self, cell: u64) -> u64 {
        load_cell(cell)
    }

    fn in_image(&self, range: &Range<u64>) -> Result<(), String> {
        self.segments.check(range)?;
        self.maps.each(range, readable)
    }

    fn in_image_all(&self, spans: &[Span]) -> Result<(), String> {
        let (mut segment, mut map) = (0, 0);
        for span in spans {
            let range = span.range();
            self.segments.check_from(&mut segment, &range)?;
            self.maps.each_from(&mut map, &range, readable)?;
        }
        Ok(())
    }

    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String> {
        self.in_image(range)?;
        super::unix::bytes(range, self.segments.base())
    }

    fn loaders(&self, at: u64, slots: u64) -> &[AtomicU64] {
        loaders(at, slots)
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        let (mut segment, mut map) = (0, 0);
        for span in writes.spans {
            self.segments.check_from(&mut segment, &span.range())?;
            let mut at = span.at;
            while at < span.end() {
                let (mapping, protection) = self.maps.mapping_from(&mut map, at)?;
                let piece = at..mapping.end.min(span.end());
                at = piece.end;
                if protection & PROT_READ == 0 {
                    // alloc: error path
                    return Err(format!("data at {:#x}, in pages with no access", piece.start));
                }
                if protection & PROT_WRITE != 0 {
                    // SAFETY: the piece is the image's (checked above) and mapped writable
                    // (read at start); other threads may use the bytes meanwhile, which
                    // `store` allows.
                    unsafe { writes.store(&piece, piece.start as *mut u8) };
                    continue;
                }
                self.aliases.store(writes, &piece, &mapping, protection)?;
            }
        }
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        for &start in starts {
            // SAFETY: a run of entries in the patch's writes ending in a zero length (the
            // switch checked), mapped and never written again (the switch's `Lasting`): the
            // unwinder reads it from now on, and nothing takes it back.
            unsafe { __register_frame(start as *const u8) };
        }
        Ok(())
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        // alloc: error path
        if functions.is_empty() { Ok(()) } else { Err("RUNTIME_FUNCTIONs on Linux".to_owned()) }
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        let to = if self.cell & PROT_WRITE != 0 {
            cell
        } else {
            let (mapping, protection) = self.maps.mapping(cell)?;
            self.aliases.writable(cell, &mapping, protection)?.0
        };
        // SAFETY: the cell is 8-aligned in the image's data, and `to` is it or where its
        // window maps the same memory writable (the offset in a window keeps the alignment of
        // the pages); a store through either is one the stubs' loads of the cell see.
        unsafe { AtomicU64::from_ptr(to as *mut u64) }.store(table, Ordering::Release);
        Ok(())
    }
}

/// Refuses a piece of pages with no access.
fn readable(piece: Range<u64>, protection: i32) -> Result<(), String> {
    if protection & PROT_READ == 0 {
        // alloc: error path
        return Err(format!("{:#x}, in pages with no access", piece.start));
    }
    Ok(())
}

/// The mappings of the process, by address, with their protections.
struct Maps(Vec<(u64, u64, i32)>);

impl Maps {
    fn read() -> Result<Maps, String> {
        let text = std::fs::read_to_string("/proc/self/maps")
            // alloc: error path
            .map_err(|it| format!("reading /proc/self/maps: {it}"))?;
        // alloc: setup, the process's mappings, read once at start
        let mut maps = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(range), Some(permissions)) = (fields.next(), fields.next()) else {
                continue;
            };
            let Some((start, end)) = range.split_once('-') else { continue };
            let (Ok(start), Ok(end)) =
                (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
            else {
                // alloc: error path
                return Err(format!("/proc/self/maps has a line {line:?}"));
            };
            let permissions = permissions.as_bytes();
            let protection = (if permissions.first() == Some(&b'r') { PROT_READ } else { 0 })
                | (if permissions.get(1) == Some(&b'w') { PROT_WRITE } else { 0 })
                | (if permissions.get(2) == Some(&b'x') { PROT_EXEC } else { 0 });
            maps.push((start, end, protection));
        }
        maps.sort_unstable();
        Ok(Maps(maps))
    }

    /// The mapping that has `at`, and its protection: a binary search.
    fn mapping(&self, at: u64) -> Result<(Range<u64>, i32), String> {
        let mut next = self.0.partition_point(|it| it.1 <= at);
        self.mapping_from(&mut next, at)
    }

    /// [`Maps::mapping`] of addresses in order, from `next` (0 at first), which only moves
    /// forward.
    fn mapping_from(&self, next: &mut usize, at: u64) -> Result<(Range<u64>, i32), String> {
        while self.0.get(*next).is_some_and(|it| it.1 <= at) {
            *next += 1;
        }
        match self.0.get(*next) {
            Some(&(start, end, protection)) if start <= at => Ok((start..end, protection)),
            // alloc: error path
            _ => Err(format!("nothing is mapped at {at:#x}")),
        }
    }

    /// Calls `f` with `range` in pieces of one mapping each, with their protections, from a
    /// binary search for the first; an error names the first address nothing maps.
    fn each(
        &self,
        range: &Range<u64>,
        f: impl FnMut(Range<u64>, i32) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut first = self.0.partition_point(|it| it.1 <= range.start);
        self.each_from(&mut first, range, f)
    }

    /// [`Maps::each`] of ranges sorted by address, one call each in order, from `next` (0 at
    /// first), which only moves forward.
    fn each_from(
        &self,
        next: &mut usize,
        range: &Range<u64>,
        mut f: impl FnMut(Range<u64>, i32) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut at = range.start;
        while self.0.get(*next).is_some_and(|it| it.1 <= at) {
            *next += 1;
        }
        for &(start, end, protection) in &self.0[*next..] {
            if at >= range.end || start > at {
                break;
            }
            let piece = at..end.min(range.end);
            at = piece.end;
            f(piece, protection)?;
        }
        if at < range.end {
            // alloc: error path
            return Err(format!("nothing is mapped at {at:#x}"));
        }
        Ok(())
    }
}

/// Makes what was written at each of `ranges`, through the addresses `delta` from them, what
/// runs there: every data cache line cleaned, one barrier, every instruction cache line
/// invalidated, one barrier. Where `CTR_EL0` says the hardware keeps either side coherent
/// itself (IDC: no clean to the point of unification; DIC: no instruction cache
/// invalidation), that side is one barrier and no line, as Linux's own
/// `caches_clean_inval_pou` does. The register is read as the kernel shows it, which hides
/// DIC on cores whose errata need the invalidation (Neoverse N1's 1542419).
#[cfg(target_arch = "aarch64")]
fn flush(ranges: impl Iterator<Item = Range<u64>> + Clone, delta: u64) {
    use std::arch::asm;
    let ctr: u64;
    // SAFETY: CTR_EL0 is readable at EL0 on Linux, which traps and emulates it where not.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let (idc, dic) = ((ctr >> 28) & 1 != 0, (ctr >> 29) & 1 != 0);
    let data_line = 4u64 << ((ctr >> 16) & 0xf);
    let code_line = 4u64 << (ctr & 0xf);
    if idc {
        // SAFETY: a barrier, which touches no memory: the stores reach the point of
        // unification, which instruction fetch is coherent with.
        unsafe { asm!("dsb ishst", options(nostack)) };
    } else {
        // Cleaning and invalidating caches by address of memory that is mapped changes no
        // memory, as __clear_cache does; each address below is of a range just written.
        // clone: an iterator over the ranges written, walked twice; it copies no element
        for range in ranges.clone() {
            let (start, end) = (range.start.wrapping_add(delta), range.end.wrapping_add(delta));
            let mut at = start & !(data_line - 1);
            while at < end {
                // SAFETY: as above: `at` is in a mapped range.
                unsafe { asm!("dc cvau, {}", in(reg) at, options(nostack)) };
                at += data_line;
            }
        }
        // SAFETY: a barrier, which touches no memory.
        unsafe { asm!("dsb ish", options(nostack)) };
    }
    if dic {
        // SAFETY: a barrier, which touches no memory: the instruction cache needs no
        // invalidation, so this thread only refetches.
        unsafe { asm!("isb", options(nostack)) };
        return;
    }
    for range in ranges {
        let mut at = range.start & !(code_line - 1);
        while at < range.end {
            // SAFETY: as above: `at` is in a mapped range.
            unsafe { asm!("ic ivau, {}", in(reg) at, options(nostack)) };
            at += code_line;
        }
    }
    // SAFETY: barriers, which touch no memory.
    unsafe { asm!("dsb ish", "isb", options(nostack)) };
}

/// x86 keeps instruction fetch coherent with stores, through any mapping of the page.
#[cfg(target_arch = "x86_64")]
fn flush(_: impl Iterator<Item = Range<u64>> + Clone, _: u64) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::unix::mprotect;

    /// Walking the mappings forward for ranges in order finds the pieces and refusals a binary
    /// search for each finds.
    #[test]
    fn a_forward_walk_of_the_maps_is_a_search_for_each() {
        let maps = Maps(vec![
            (0x1000, 0x3000, PROT_READ),
            (0x3000, 0x4000, PROT_READ | PROT_WRITE),
            (0x6000, 0x9000, 0),
            (0x9000, 0xa000, PROT_READ | PROT_EXEC),
        ]);
        let ranges = [
            0x1000..0x1008,
            0x2ff8..0x3008,
            0x3100..0x3200,
            0x3ff0..0x4010,
            0x6100..0x6108,
            0x9000..0x9ff8,
            0xa000..0xa008,
        ];
        let mut next = 0;
        for range in &ranges {
            let (mut searched, mut walked) = (Vec::new(), Vec::new());
            let by_search = maps.each(range, |piece, protection| {
                searched.push((piece, protection));
                readable(0..1, protection)
            });
            let by_walk = maps.each_from(&mut next, range, |piece, protection| {
                walked.push((piece, protection));
                readable(0..1, protection)
            });
            assert_eq!(searched, walked, "{range:x?}");
            assert_eq!(by_search.is_ok(), by_walk.is_ok(), "{range:x?}");
            let mut from = 0;
            for at in [range.start, range.end - 1] {
                assert_eq!(maps.mapping(at), maps.mapping_from(&mut from, at), "{at:#x}");
            }
        }
    }

    #[test]
    fn a_window_is_aligned_and_within_its_mapping_and_the_room_left() {
        const W: u64 = WINDOW;
        let mapping = 2 * W - 0x3000..4 * W + 0x1000;
        let all = 0..u64::MAX;
        assert_eq!(window(2 * W + 0x10, &mapping, all.clone()), 2 * W..3 * W);
        assert_eq!(window(2 * W - 0x1000, &mapping, all.clone()), mapping.start..2 * W);
        assert_eq!(window(4 * W + 0x800, &mapping, all), 4 * W..mapping.end);
        let free = 2 * W + 0x4_0000..3 * W - 0x4_0000;
        assert_eq!(window(2 * W + W / 2, &mapping, free.clone()), free);
        for at in [mapping.start, 2 * W, 3 * W - 1, 3 * W + 0xa_b123, mapping.end - 1] {
            let found = window(at, &mapping, 0..u64::MAX);
            assert!(found.contains(&at), "{at:#x} in {found:x?}");
            assert!(found.start >= mapping.start && found.end <= mapping.end);
            assert!(found.end - found.start <= WINDOW);
            assert!(found.start.is_multiple_of(0x1000) && found.end.is_multiple_of(0x1000));
        }
    }

    /// Writes into read-only pages land where the program reads them, through windows made
    /// once each, and leave the pages read-only.
    #[test]
    fn read_only_pages_are_written_through_a_window_and_stay_read_only() {
        const MAP_PRIVATE: i32 = 2;
        const MAP_ANONYMOUS: i32 = 0x20;
        // SAFETY: it takes no pointer.
        let page = unsafe { sysconf(SC_PAGESIZE) } as usize;
        let len = 4 * page;
        let flags = MAP_PRIVATE | MAP_ANONYMOUS;
        let prot = PROT_READ | PROT_WRITE;
        // SAFETY: a new private mapping anywhere, which only this test uses.
        let at = unsafe { mmap(std::ptr::null_mut(), len, prot, flags, -1, 0) };
        assert_ne!(at, MAP_FAILED);
        let base = at as u64;
        let mut expected: Vec<u8> = (0..len).map(|it| (it * 7) as u8).collect();
        // SAFETY: `len` bytes mapped writable above, which only this test uses.
        unsafe { std::ptr::copy_nonoverlapping(expected.as_ptr(), at as *mut u8, len) };
        // SAFETY: as above.
        assert_eq!(unsafe { mprotect(at, len, PROT_READ) }, 0);
        let mapping = base..base + len as u64;

        let message: Vec<u8> = (0..64u8).map(|it| it ^ 0xa5).collect();
        let (first, second) = (page + 4, 3 * page - 8);
        let spans =
            [Span::new(base + first as u64, 0, 24), Span::new(base + second as u64, 24, 16)];
        let writes = Writes::new(&message, &spans, &[], 0);
        let mut aliases = Aliases::new();
        for span in &spans {
            aliases.store(&writes, &span.range(), &mapping, PROT_READ).expect("written");
        }
        let blocks = |at: u64, len: u64| [at / WINDOW, (at + len - 1) / WINDOW];
        let mut touched: Vec<u64> =
            spans.iter().flat_map(|it| blocks(it.at, it.len as u64)).collect();
        touched.sort_unstable();
        touched.dedup();
        assert_eq!(aliases.windows.len(), touched.len(), "a window per block written, once");
        expected[first..first + 24].copy_from_slice(&message[..24]);
        expected[second..second + 16].copy_from_slice(&message[24..40]);
        // SAFETY: the mapping is readable, and nothing writes it while this reads.
        let now = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
        assert_eq!(now, &expected[..]);

        let again = [Span::new(base + first as u64 + 32, 40, 8)];
        let writes = Writes::new(&message, &again, &[], 0);
        aliases.store(&writes, &again[0].range(), &mapping, PROT_READ).expect("written");
        assert_eq!(aliases.windows.len(), touched.len(), "a second write makes no window");
        let (to, _) = aliases.writable(base + 8, &mapping, PROT_READ).expect("a window");
        // SAFETY: 8-aligned (the window keeps the page offset), mapped writable for the test.
        unsafe { AtomicU64::from_ptr(to as *mut u64) }.store(0x1234, Ordering::Release);
        // SAFETY: the mapping is readable and 8-aligned at `base + 8`.
        assert_eq!(
            unsafe { AtomicU64::from_ptr((base + 8) as *mut u64) }.load(Ordering::Acquire),
            0x1234
        );

        Maps::read()
            .expect("this process's maps")
            .each(&mapping, |piece, protection| {
                assert_eq!(protection & (PROT_WRITE | PROT_EXEC), 0, "{piece:x?} stays read-only");
                Ok(())
            })
            .expect("still mapped");
        for &(start, end, alias) in &aliases.windows {
            // SAFETY: the window's second mapping, which only this test uses.
            unsafe { munmap(alias as *mut c_void, (end - start) as usize) };
        }
        // SAFETY: as above, the mapping the windows were moved over.
        unsafe { munmap(at, len) };
    }

    #[test]
    fn maps_are_found_by_binary_search_in_pieces() {
        let maps =
            Maps(vec![(0x1000, 0x3000, PROT_READ), (0x3000, 0x4000, PROT_READ | PROT_WRITE)]);
        let mut pieces = Vec::new();
        maps.each(&(0x2ff8..0x3008), |piece, protection| {
            pieces.push((piece, protection));
            Ok(())
        })
        .expect("mapped");
        assert_eq!(
            pieces,
            vec![(0x2ff8..0x3000, PROT_READ), (0x3000..0x3008, PROT_READ | PROT_WRITE)]
        );
        assert!(maps.each(&(0x3ff8..0x4008), |_, _| Ok(())).unwrap_err().contains("0x4000"));
        assert!(maps.each(&(0x800..0x1008), |_, _| Ok(())).unwrap_err().contains("0x800"));
        let mine = Maps::read().expect("this process's maps");
        let local = std::hint::black_box(0u64);
        let stack = &local as *const u64 as u64;
        mine.each(&(stack..stack + 8), |_, protection| {
            assert_eq!(protection & (PROT_READ | PROT_WRITE), PROT_READ | PROT_WRITE);
            Ok(())
        })
        .expect("the stack is mapped");
    }
}
