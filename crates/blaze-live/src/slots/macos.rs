//! Writing a running Mach-O image's data on macOS AArch64.
//!
//! The patch code region is the daemon's code memory (blaze-inproc), mapped by the start hook
//! before `main`: the commit writes the code there, and nothing here maps over the region or
//! writes it.
//!
//! A page that is not writable is never made writable, since a reader of it may be another
//! thread. A copy of its pages is written, given their protection, and mapped over them in
//! one `mach_vm_remap`, then the instruction cache is invalidated (`sys_icache_invalidate`)
//! when they run. The protections of the image's regions are read once, at start
//! ([`Regions`]): a swap gives pages the protection they had, so they stay true, and checking
//! a patch's writes takes no system call. Data in writable pages is written in place with
//! atomic stores ([`Writes::store`]), since other threads may read it meanwhile.

#![allow(
    unsafe_code,
    reason = "the image's protections, its read-only pages' copies and unwind registration"
)]

use std::{
    ffi::c_void,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) use super::unix::{GRAIN, Reserved, backtrace, descriptor, in_image_of};
use super::{
    switch::{Dwarf, Layout, Memory, Writes},
    unix::{__register_frame, Segments, load_cell, loaders},
};

unsafe extern "C" {
    fn mach_task_self() -> u32;
    fn mach_vm_allocate(task: u32, address: *mut u64, size: u64, flags: i32) -> i32;
    fn mach_vm_deallocate(task: u32, address: u64, size: u64) -> i32;
    fn mach_vm_protect(task: u32, address: u64, size: u64, set_max: i32, protection: i32) -> i32;
    fn mach_vm_remap(
        target: u32,
        address: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        source_task: u32,
        source: u64,
        copy: i32,
        protection: *mut i32,
        max_protection: *mut i32,
        inheritance: u32,
    ) -> i32;
    fn mach_vm_region(
        task: u32,
        address: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
        object: *mut u32,
    ) -> i32;
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

const READ: i32 = 1;
const WRITE: i32 = 2;
const EXECUTE: i32 = 4;
const PAGE: u64 = 0x4000;
const ANYWHERE: i32 = 1;
const FIXED: i32 = 0;
const OVERWRITE: i32 = 0x4000;
const INHERIT_NONE: u32 = 2;
const BASIC_INFO_64: i32 = 9;

pub(super) struct Os {
    /// The protection of the table cell's region.
    cell: i32,
    /// Where the image is mapped, which every data write is checked to be in.
    segments: Segments,
    regions: Regions,
}

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        let (_, cell) = region_at(layout.cell())?;
        let segments = Segments::of(layout.cell())?;
        let regions = Regions::read(&segments.hull())?;
        Ok(Os { cell, segments, regions })
    }
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Entries;
    const SEH: bool = false;

    fn cell(&self, cell: u64) -> u64 {
        load_cell(cell)
    }

    fn in_image(&self, range: &Range<u64>) -> Result<(), String> {
        self.segments.check(range)?;
        let mut at = range.start;
        while at < range.end {
            let (region, protection) = self.regions.at(at)?;
            if protection & READ == 0 {
                // alloc: error path
                return Err(format!("{at:#x}, in pages with no access"));
            }
            at = region.end;
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
        let mut run: Option<(Range<u64>, i32)> = None;
        for span in writes.spans {
            self.segments.check(&span.range())?;
            let mut at = span.at;
            while at < span.end() {
                let (found, protection) = self.regions.at(at)?;
                let piece = at..found.end.min(span.end());
                at = piece.end;
                if protection & READ == 0 {
                    // alloc: error path
                    return Err(format!("data at {:#x}, in pages with no access", piece.start));
                }
                if protection & WRITE != 0 {
                    // SAFETY: the image's (checked above), mapped and writable, and stays
                    // mapped; other threads may use the bytes meanwhile, which `store` allows.
                    unsafe { writes.store(&piece, piece.start as *mut u8) };
                    continue;
                }
                let pages = pages(&piece);
                match &mut run {
                    Some((pending, same)) if *same == protection && pages.start <= pending.end => {
                        pending.end = pending.end.max(pages.end);
                    }
                    _ => {
                        if let Some((done, protection)) = run.replace((pages, protection)) {
                            // SAFETY: read-only pages of the image (checked above), which nothing
                            // writes; the copy mapped over them in one step has the rest of
                            // their bytes as they are, so a reader sees the old page or the new.
                            unsafe { swap(&done, protection, |copy| writes.fill(&done, copy))? };
                        }
                    }
                }
            }
        }
        if let Some((done, protection)) = run {
            // SAFETY: as above.
            unsafe { swap(&done, protection, |copy| writes.fill(&done, copy))? };
        }
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        for &start in starts {
            // SAFETY: an FDE in the patch's writes (the switch checked), mapped and never
            // written again (the switch's `Lasting`): the unwinder reads it from now on, and
            // nothing takes it back.
            unsafe { __register_frame(start as *const u8) };
        }
        Ok(())
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        // alloc: error path
        if functions.is_empty() { Ok(()) } else { Err("RUNTIME_FUNCTIONs on macOS".to_owned()) }
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        if self.cell & WRITE != 0 {
            // SAFETY: the cell is 8-aligned in the image's data, and writable.
            unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
            return Ok(());
        }
        // A page mapped over the cell's in one step is a store a reader sees all of or none.
        let pages = pages(&(cell..cell + 8));
        let bytes = table.to_le_bytes();
        let at = (cell - pages.start) as usize;
        // SAFETY: the cell's page is read-only data of the image, which nothing else writes.
        unsafe { swap(&pages, self.cell, |copy| copy[at..at + 8].copy_from_slice(&bytes)) }
    }
}

fn pages(range: &Range<u64>) -> Range<u64> {
    range.start & !(PAGE - 1)..range.end.next_multiple_of(PAGE)
}

fn check(code: i32) -> Result<(), String> {
    // alloc: error path
    if code == 0 { Ok(()) } else { Err(format!("kern_return {code}")) }
}

/// The image's mapped regions and their protections, sorted, read once at start: a lookup
/// is a binary search where `mach_vm_region` is a trap.
struct Regions(Vec<(u64, u64, i32)>);

impl Regions {
    /// The regions of `image`, cut to it.
    fn read(image: &Range<u64>) -> Result<Regions, String> {
        // alloc: setup, the image's regions, read once at start
        let mut regions = Vec::with_capacity(16);
        let mut at = image.start;
        while at < image.end {
            let Some((found, protection)) = next_region(at)? else { break };
            if found.start >= image.end {
                break;
            }
            regions.push((found.start.max(image.start), found.end.min(image.end), protection));
            at = found.end;
        }
        Ok(Regions(regions))
    }

    /// The region that has `at`, and its protection.
    fn at(&self, at: u64) -> Result<(Range<u64>, i32), String> {
        let next = self.0.partition_point(|it| it.1 <= at);
        match self.0.get(next) {
            Some(&(start, end, protection)) if start <= at => Ok((start..end, protection)),
            // alloc: error path
            _ => Err(format!("nothing is mapped at {at:#x}")),
        }
    }
}

/// The mapped region at `address` and its protection.
fn region_at(address: u64) -> Result<(Range<u64>, i32), String> {
    match next_region(address)? {
        Some((region, protection)) if region.contains(&address) => Ok((region, protection)),
        // alloc: error path
        _ => Err(format!("nothing is mapped at {address:#x}")),
    }
}

/// The first mapped region that ends past `address`, and its protection; `None` when there
/// is none.
fn next_region(address: u64) -> Result<Option<(Range<u64>, i32)>, String> {
    const KERN_INVALID_ADDRESS: i32 = 1;
    // SAFETY: it takes no pointer.
    let task = unsafe { mach_task_self() };
    let (mut start, mut size, mut object) = (address, 0u64, 0u32);
    let mut info = [0i32; 9];
    let mut count = info.len() as u32;
    // SAFETY: `info` is as large as the basic information of a region, in `int`s.
    let code = unsafe {
        mach_vm_region(
            task,
            &mut start,
            &mut size,
            BASIC_INFO_64,
            info.as_mut_ptr(),
            &mut count,
            &mut object,
        )
    };
    if code == KERN_INVALID_ADDRESS {
        return Ok(None);
    }
    // alloc: error path
    check(code).map_err(|it| format!("reading the protection at {address:#x}: {it}"))?;
    Ok(Some((start..start.saturating_add(size), info[0])))
}

/// Writes the whole pages `pages`, mapped with `protection`, by mapping a copy of them over
/// them: `fill` writes the copy, which stands for `pages`.
///
/// # Safety
///
/// `pages` are whole mapped and readable pages of the image that nothing writes while this
/// runs.
unsafe fn swap(
    pages: &Range<u64>,
    protection: i32,
    fill: impl FnOnce(&mut [u8]),
) -> Result<(), String> {
    let len = pages.end - pages.start;
    // SAFETY: it takes no pointer.
    let task = unsafe { mach_task_self() };
    let mut scratch = 0u64;
    // SAFETY: a new allocation anywhere, written to `scratch`.
    check(unsafe { mach_vm_allocate(task, &mut scratch, len, ANYWHERE) })
        // alloc: error path
        .map_err(|it| format!("allocating a copy of {:#x}: {it}", pages.start))?;
    {
        // SAFETY: `len` bytes allocated above, zeroed, which only this function knows of.
        let copy = unsafe { std::slice::from_raw_parts_mut(scratch as *mut u8, len as usize) };
        // SAFETY: `pages` are mapped and readable, and nothing writes them (the caller's).
        let now = unsafe { std::slice::from_raw_parts(pages.start as *const u8, len as usize) };
        copy.copy_from_slice(now);
        fill(copy);
    }
    let swapped = (|| {
        // SAFETY: the allocation above, of `len` bytes.
        check(unsafe { mach_vm_protect(task, scratch, len, 0, protection) })?;
        let (mut current, mut max, mut target) = (0, 0, pages.start);
        // SAFETY: maps the copy over `pages` in one step, so a reader sees the old bytes or
        // the new; whole pages (the caller's), with the protection they had.
        check(unsafe {
            mach_vm_remap(
                task,
                &mut target,
                len,
                0,
                FIXED | OVERWRITE,
                task,
                scratch,
                0,
                &mut current,
                &mut max,
                INHERIT_NONE,
            )
        })
    })();
    // SAFETY: the allocation above, which no slice borrows now; the remap shares its pages.
    unsafe { mach_vm_deallocate(task, scratch, len) };
    // alloc: error path
    swapped.map_err(|it| format!("writing at {:#x}: {it}", pages.start))?;
    if protection & EXECUTE != 0 {
        // SAFETY: the pages were just mapped; invalidating changes no memory.
        unsafe { sys_icache_invalidate(pages.start as *mut c_void, len as usize) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::unix::{mmap, mprotect, munmap};

    /// The regions read once give, for every address, what `mach_vm_region` gives for it.
    #[test]
    fn regions_read_at_start_say_what_the_kernel_says() {
        const MAP_PRIVATE: std::ffi::c_int = 2;
        const MAP_ANONYMOUS: std::ffi::c_int = 0x1000;
        let len = 3 * PAGE as usize;
        // SAFETY: a new private mapping anywhere, which only this test uses.
        let at = unsafe {
            mmap(std::ptr::null_mut(), len, READ | WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
        };
        assert_ne!(at, !0usize as *mut c_void);
        let base = at as u64;
        // SAFETY: the middle page of the mapping above.
        assert_eq!(unsafe { mprotect((base + PAGE) as *mut c_void, PAGE as usize, READ) }, 0);
        let image = base..base + len as u64;
        let regions = Regions::read(&image).expect("read");
        for at in (image.start..image.end).step_by(PAGE as usize / 4) {
            let (found, protection) = regions.at(at).expect("mapped");
            let (kernel, said) = region_at(at).expect("mapped");
            assert_eq!(protection, said, "{at:#x}");
            assert!(found.contains(&at) && found.start >= kernel.start.max(image.start));
            assert!(found.end <= kernel.end.min(image.end));
        }
        assert_eq!(regions.at(base + PAGE + 8).expect("mapped").1 & WRITE, 0);
        assert!(regions.at(image.end).is_err());
        // SAFETY: the mapping above, which nothing borrows.
        unsafe { munmap(at, len) };
    }
}
