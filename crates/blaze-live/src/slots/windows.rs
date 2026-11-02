//! Writing a running PE image on Windows x86-64.
//!
//! The patch code region is the daemon's code memory (blaze-inproc), mapped by the start hook
//! before the CRT starts: the commit writes the code there, and nothing here writes it.
//!
//! Pages of data are made writable for a write with `VirtualProtect` and given their
//! protection back, once per run of pages of one protection that a patch writes; pages
//! already writable are written in place with no system call. The image's regions and protections are read once, at start
//! ([`Regions`]): every write gives pages back the protection they had, so they stay true,
//! and checking a patch's writes takes no `VirtualQuery`.
//! Unwinding goes by `RUNTIME_FUNCTION`s, added with `RtlAddFunctionTable`. Data is written
//! with atomic stores ([`Writes::store`]), since other threads may read it meanwhile. Address
//! space for the switch's columns is reserved and committed with `VirtualAlloc`
//! ([`Reserved`]).

#![allow(unsafe_code, reason = "the image's protections, data writes and unwind registration")]

use std::{
    ffi::c_void,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    image,
    switch::{Dwarf, Layout, Memory, Writes},
};
use crate::patch::{DESCRIPTOR, Descriptor};

#[repr(C)]
#[allow(dead_code)]
struct MemoryBasicInformation {
    base: *mut c_void,
    allocation_base: *mut c_void,
    allocation_protect: u32,
    partition: u16,
    size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

#[repr(C)]
#[allow(dead_code)]
struct RuntimeFunction {
    begin: u32,
    end: u32,
    unwind: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
    fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32;
    fn VirtualQuery(address: *const c_void, info: *mut MemoryBasicInformation, len: usize)
    -> usize;
    fn FlushInstructionCache(process: *mut c_void, address: *const c_void, size: usize) -> i32;
    fn RtlAddFunctionTable(table: *const RuntimeFunction, count: u32, base: u64) -> u8;
    fn RtlCaptureStackBackTrace(
        skip: u32,
        count: u32,
        frames: *mut *mut c_void,
        hash: *mut u32,
    ) -> u16;
}

const FROM_ADDRESS: u32 = 0x4;
const UNCHANGED_REFCOUNT: u32 = 0x2;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_GUARD: u32 = 0x100;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
const EXECUTABLE: u32 = 0xf0;
const WRITABLE: u32 =
    PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
const PAGE: u64 = 0x1000;
/// What `RtlCaptureStackBackTrace` takes at most on every Windows.
const FRAMES: usize = 62;

/// Where the image that has `address` is loaded.
fn module_of(address: *const c_void) -> Result<*mut c_void, String> {
    let mut module = std::ptr::null_mut();
    // SAFETY: with FROM_ADDRESS the name is an address in the module; the count of
    // references is left alone, and the image stays loaded as long as its code runs.
    let found =
        unsafe { GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, address as _, &mut module) };
    if found == 0 || module.is_null() {
        // alloc: error path
        return Err(format!("no module has {address:?}: {}", std::io::Error::last_os_error()));
    }
    Ok(module)
}

/// Where the descriptor of the module this code is in is, in memory, and its bytes.
pub(super) fn descriptor() -> Result<(u64, &'static [u8]), String> {
    let own = descriptor as fn() -> Result<(u64, &'static [u8]), String> as *const c_void;
    let module = module_of(own)?;
    // alloc: setup, the descriptor's name, looked up once at start
    let name: Vec<u8> = DESCRIPTOR.bytes().chain([0]).collect();
    // SAFETY: the module is loaded and the name ends in a zero.
    let found = unsafe { GetProcAddress(module, name.as_ptr()) };
    if found.is_null() {
        // alloc: error path
        return Err(format!(
            "the module of blaze-live's code exports no {DESCRIPTOR}: blaze-link links it for \
             live patches"
        ));
    }
    let at = found as u64;
    let end =
        at.checked_add(Descriptor::SIZE as u64).ok_or("a descriptor past the end of memory")?;
    Ok((at, readable(&(at..end), module as u64)?))
}

/// Checks that `range` is committed, readable memory of the module that has `near` (the
/// descriptor's address).
pub(super) fn in_image_of(range: &Range<u64>, near: u64) -> Result<(), String> {
    let module = module_of(near as *const c_void)?;
    readable(range, module as u64).map(|_| ())
}

/// The image's bytes at `range`, checked to be committed, readable and of the image at
/// `base`.
fn readable(range: &Range<u64>, base: u64) -> Result<&'static [u8], String> {
    regions(range, |piece, region| checked_readable(&piece, region, base))?;
    // SAFETY: `range` is committed, readable memory of the image (checked above), which stays
    // loaded as long as its code, this, runs; it is data the linker wrote, which no patch
    // writes over (the switch refuses data over the descriptor's cell and table, and the
    // descriptor and the directory name are read-only data).
    Ok(unsafe { image::view(range.start, (range.end - range.start) as usize) })
}

/// Checks that `piece`, of `region`, is readable memory of the image at `base`.
fn checked_readable(piece: &Range<u64>, region: Region, base: u64) -> Result<(), String> {
    if region.allocation != base {
        // alloc: error path
        return Err(format!("{:#x}, outside the image at {base:#x}", piece.start));
    }
    let protect = region.protect & 0xff;
    if protect == PAGE_NOACCESS || region.protect & PAGE_GUARD != 0 {
        // alloc: error path
        return Err(format!("{:#x}, in pages with no access", piece.start));
    }
    Ok(())
}

/// Pushes the code address of each frame of this thread's stack, newest first.
pub(super) fn backtrace(pcs: &mut Vec<u64>) {
    let mut frames = [std::ptr::null_mut(); FRAMES];
    // SAFETY: `frames` has room for `FRAMES`; no hash is asked for.
    let count = unsafe {
        RtlCaptureStackBackTrace(0, FRAMES as u32, frames.as_mut_ptr(), std::ptr::null_mut())
    };
    let count = usize::from(count).min(FRAMES);
    pcs.extend(frames[..count].iter().map(|it| (*it as u64).wrapping_sub(1)));
}

pub(super) struct Os {
    /// Where the image is loaded, which `RUNTIME_FUNCTION`s are relative to.
    base: u64,
    regions: Regions,
}

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        let base = module_of(layout.cell() as *const c_void)? as u64;
        let regions = Regions::read(base)?;
        let code = layout.code_in_memory();
        if !code.is_empty() {
            regions.each(&code, |_, _| Ok(()))?;
        }
        Ok(Os { base, regions })
    }

    /// Writes data anywhere in the image with [`Writes::store`], since other threads may read
    /// or write it meanwhile.
    fn write(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        let base = self.base;
        let mut run: Option<(Range<u64>, Region)> = None;
        for span in writes.spans {
            self.regions.each(&span.range(), |piece, region| {
                if region.allocation != base {
                    // alloc: error path
                    return Err(format!(
                        "a write at {:#x}..{:#x}, outside the image at {base:#x}",
                        piece.start, piece.end
                    ));
                }
                let protect = region.protect & 0xff;
                if protect == PAGE_NOACCESS || region.protect & PAGE_GUARD != 0 {
                    // alloc: error path
                    return Err(format!("a write at {:#x}, in pages with no access", piece.start));
                }
                if protect & WRITABLE != 0 {
                    // SAFETY: the piece is the image's, committed and writable (`Regions` read
                    // it), and stays committed; `store` allows other threads to use the bytes.
                    unsafe { writes.store(&piece, piece.start as *mut u8) };
                    if protect & EXECUTABLE != 0 {
                        flush(&piece);
                    }
                    return Ok(());
                }
                let pages = piece.start & !(PAGE - 1)..piece.end.next_multiple_of(PAGE);
                // One `VirtualProtect` pair covers pages of one allocation with one protection.
                match &mut run {
                    Some((pending, same)) if *same == region && pages.start <= pending.end => {
                        pending.end = pending.end.max(pages.end);
                    }
                    _ => {
                        if let Some((done, region)) = run.replace((pages, region)) {
                            protected(writes, &done, region.protect & 0xff)?;
                        }
                    }
                }
                Ok(())
            })?;
        }
        if let Some((done, region)) = run {
            protected(writes, &done, region.protect & 0xff)?;
        }
        Ok(())
    }
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Not;
    const SEH: bool = true;

    fn cell(&self, cell: u64) -> u64 {
        // SAFETY: the cell is the descriptor's, 8-aligned (`Layout::new` checked), checked to
        // be in the image by `Switch::new` before the first load, and only ever accessed
        // atomically.
        unsafe { image::cell(cell) }.load(Ordering::Acquire)
    }

    fn in_image(&self, range: &Range<u64>) -> Result<(), String> {
        let base = self.base;
        self.regions.each(range, |piece, region| checked_readable(&piece, region, base))
    }

    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String> {
        self.in_image(range)?;
        // SAFETY: as in `readable`: committed, readable memory of the image (checked above)
        // that no patch writes over.
        Ok(unsafe { image::view(range.start, (range.end - range.start) as usize) })
    }

    fn loaders(&self, at: u64, slots: u64) -> &[AtomicU64] {
        // SAFETY: `Switch::new` checked `at..at + slots * 8` is in the image and `at` 8-aligned
        // before the first call; it is the image's writable data (the linker's `Class::Table`),
        // which only `publish`'s atomic stores write (the switch refuses data over it).
        unsafe { image::words(at, slots as usize) }
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        self.write(writes)
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        // alloc: error path
        if starts.is_empty() { Ok(()) } else { Err("DWARF unwind entries on Windows".to_owned()) }
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        if functions.is_empty() {
            return Ok(());
        }
        let relative = |address: u64| {
            address.checked_sub(self.base).and_then(|it| u32::try_from(it).ok()).ok_or_else(|| {
                // alloc: error path
                format!("{address:#x}, out of reach of the image at {:#x}", self.base)
            })
        };
        let mut table = functions
            .iter()
            .map(|&(begin, end, unwind)| {
                Ok(RuntimeFunction {
                    begin: relative(begin)?,
                    end: relative(end)?,
                    unwind: relative(unwind)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        table.sort_by_key(|it| it.begin);
        let count = u32::try_from(table.len()).map_err(|_| "too many RUNTIME_FUNCTIONs")?;
        // The unwinder reads the table from now on, and the code and unwind information it
        // names are never written again (the switch's `Lasting`): it is never freed.
        let table: &'static [RuntimeFunction] = Box::leak(table.into_boxed_slice());
        // SAFETY: `count` entries, sorted, relative to `base`, alive forever.
        if unsafe { RtlAddFunctionTable(table.as_ptr(), count, self.base) } == 0 {
            // alloc: error path
            return Err("the unwinder refused the patch's RUNTIME_FUNCTIONs".to_owned());
        }
        Ok(())
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        let range = cell..cell + 8;
        let (mut protect, mut count) = (0, 0);
        self.regions.each(&range, |_, region| {
            protect = region.protect & 0xff;
            count += 1;
            Ok(())
        })?;
        if count != 1 {
            // alloc: error path
            return Err(format!("the table cell at {cell:#x} spans two regions"));
        }
        let store = || {
            // SAFETY: the cell is 8-aligned in the image's data, and writable when this runs.
            unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
        };
        if protect & WRITABLE != 0 {
            store();
            return Ok(());
        }
        // SAFETY: the cell is the image's, committed; writable for the store alone.
        unsafe { writable(&range, protect, store) }
    }
}

/// What a run of pages written with one `VirtualProtect` pair has the same of.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Region {
    allocation: u64,
    /// With its modifiers (`PAGE_GUARD` and the rest).
    protect: u32,
}

/// The image's committed regions and their protections, sorted, read once at start by
/// walking `VirtualQuery` from the image's base for as long as the allocation is the
/// image's: a lookup is a binary search where `VirtualQuery` is a system call.
struct Regions(Vec<(u64, u64, Region)>);

impl Regions {
    fn read(base: u64) -> Result<Regions, String> {
        // alloc: setup, the image's regions, read once at start
        let mut regions = Vec::with_capacity(16);
        let mut at = base;
        loop {
            let mut info = MemoryBasicInformation {
                base: std::ptr::null_mut(),
                allocation_base: std::ptr::null_mut(),
                allocation_protect: 0,
                partition: 0,
                size: 0,
                state: 0,
                protect: 0,
                kind: 0,
            };
            let len = size_of::<MemoryBasicInformation>();
            // SAFETY: `info` is a whole MEMORY_BASIC_INFORMATION.
            if unsafe { VirtualQuery(at as *const c_void, &mut info, len) } != len {
                break;
            }
            let end = (info.base as u64).saturating_add(info.size as u64);
            if info.allocation_base as u64 != base || end <= at {
                break;
            }
            if info.state == MEM_COMMIT {
                let region = Region { allocation: base, protect: info.protect };
                regions.push((info.base as u64, end, region));
            }
            at = end;
        }
        if regions.is_empty() {
            // alloc: error path, at start
            return Err(format!("nothing of the image at {base:#x} is committed"));
        }
        Ok(Regions(regions))
    }

    /// Calls `f` with `range` in pieces of one region each, from a binary search for the
    /// first; an error names the first address of it nothing is committed at.
    fn each(
        &self,
        range: &Range<u64>,
        mut f: impl FnMut(Range<u64>, Region) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut at = range.start;
        let mut next = self.0.partition_point(|it| it.1 <= at);
        while at < range.end {
            match self.0.get(next) {
                Some(&(start, end, region)) if start <= at => {
                    let piece = at..end.min(range.end);
                    at = piece.end;
                    next += 1;
                    f(piece, region)?;
                }
                // alloc: error path
                _ => return Err(format!("nothing is committed at {at:#x}")),
            }
        }
        Ok(())
    }
}

/// Calls `f` with `range` in pieces of one region each; an error names the first address
/// of it nothing is committed at.
fn regions(
    range: &Range<u64>,
    f: impl FnMut(Range<u64>, Region) -> Result<(), String>,
) -> Result<(), String> {
    regions_from(&mut None, range, f)
}

/// [`regions`], starting from `known`, the region last queried, and leaving there the last
/// one it queries: spans sorted by address in one region cost one `VirtualQuery` in all. A
/// region's protection is the same after a write as before ([`writable`] gives it back), so
/// what was queried stays true while a patch is written.
fn regions_from(
    known: &mut Option<(Range<u64>, Region)>,
    range: &Range<u64>,
    mut f: impl FnMut(Range<u64>, Region) -> Result<(), String>,
) -> Result<(), String> {
    let mut at = range.start;
    while at < range.end {
        if let Some((region, of)) = known.as_ref().filter(|it| it.0.contains(&at)) {
            let (piece, of) = (at..region.end.min(range.end), *of);
            at = piece.end;
            f(piece, of)?;
            continue;
        }
        let mut info = MemoryBasicInformation {
            base: std::ptr::null_mut(),
            allocation_base: std::ptr::null_mut(),
            allocation_protect: 0,
            partition: 0,
            size: 0,
            state: 0,
            protect: 0,
            kind: 0,
        };
        let len = size_of::<MemoryBasicInformation>();
        // SAFETY: `info` is a whole MEMORY_BASIC_INFORMATION.
        if unsafe { VirtualQuery(at as *const c_void, &mut info, len) } != len {
            // alloc: error path
            return Err(format!("querying {at:#x}: {}", std::io::Error::last_os_error()));
        }
        let end = (info.base as u64).saturating_add(info.size as u64);
        if info.state != MEM_COMMIT || end <= at {
            // alloc: error path
            return Err(format!("nothing is committed at {at:#x}"));
        }
        let region = Region { allocation: info.allocation_base as u64, protect: info.protect };
        *known = Some((info.base as u64..end, region));
        let piece = at..end.min(range.end);
        at = piece.end;
        f(piece, region)?;
    }
    Ok(())
}

/// Writes what `writes` has within `pages`, committed memory of the image with `protect`,
/// which isn't writable: made writable once for the write, then given it back.
fn protected(writes: &Writes<'_>, pages: &Range<u64>, protect: u32) -> Result<(), String> {
    let put = || {
        // SAFETY: the pages are the image's, committed and writable while this runs
        // (`writable`); `store` allows other threads to use the bytes.
        unsafe { writes.store(pages, pages.start as *mut u8) }
    };
    // SAFETY: the pages are the image's, committed with `protect` (`regions` read it).
    unsafe { writable(pages, protect, put)? };
    if protect & EXECUTABLE != 0 {
        flush(pages);
    }
    Ok(())
}

fn flush(range: &Range<u64>) {
    // SAFETY: it takes no pointer; the handle stands for this process and needs no closing.
    let process = unsafe { GetCurrentProcess() };
    let (start, len) = (range.start as *const c_void, (range.end - range.start) as usize);
    // SAFETY: the range was just written; flushing changes no memory.
    unsafe { FlushInstructionCache(process, start, len) };
}

/// Runs `f` with `range`, of regions with `protect`, made writable.
///
/// # Safety
///
/// `range` is committed memory of the image that nothing else changes the protection of.
unsafe fn writable(range: &Range<u64>, protect: u32, f: impl FnOnce()) -> Result<(), String> {
    let wanted = if protect & EXECUTABLE != 0 { PAGE_EXECUTE_READWRITE } else { PAGE_READWRITE };
    let (address, len) = (range.start as *mut c_void, (range.end - range.start) as usize);
    let mut old = 0;
    // SAFETY: the caller's.
    if unsafe { VirtualProtect(address, len, wanted, &mut old) } == 0 {
        // alloc: error path
        return Err(format!(
            "making {:#x} writable: {}",
            range.start,
            std::io::Error::last_os_error()
        ));
    }
    f();
    let mut was = 0;
    // SAFETY: as above; the regions had one protection, which they get back.
    if unsafe { VirtualProtect(address, len, old, &mut was) } == 0 {
        // alloc: error path
        return Err(format!(
            "protecting {:#x} again: {}",
            range.start,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// What a reservation's length and commits are multiples of: the allocation granularity
/// (64 KiB), so every commit is whole pages.
pub(super) const GRAIN: usize = 64 << 10;

/// Address space reserved with no access and no memory behind it (`MEM_RESERVE`), committed
/// read and write from its start as it grows (`MEM_COMMIT`), never moved, and released when
/// dropped. A committed page is charged to the commit limit, and costs memory only once
/// written.
///
/// Invariant, which [`image::rows`] relies on: `at..at + committed` is committed read and
/// write, zero where never written, and no one else's; `at` is page-aligned.
pub(super) struct Reserved {
    at: u64,
    len: usize,
    committed: usize,
}

impl Reserved {
    /// Reserves `len` bytes, a nonzero multiple of [`GRAIN`].
    pub(super) fn new(len: usize) -> Result<Reserved, String> {
        assert!(len != 0 && len.is_multiple_of(GRAIN), "a reservation of whole grains");
        // SAFETY: a new reservation anywhere, with no access: it changes no memory there is.
        let at = unsafe { VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE, PAGE_NOACCESS) };
        if at.is_null() {
            // alloc: error path, once at start
            return Err(format!(
                "VirtualAlloc of {len} bytes of address space: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Reserved { at: at as u64, len, committed: 0 })
    }

    pub(super) fn base(&self) -> u64 {
        self.at
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn committed(&self) -> usize {
        self.committed
    }

    /// Commits the first `upto` bytes, a multiple of [`GRAIN`] no more than the length: one
    /// `VirtualAlloc` of what was not committed yet.
    pub(super) fn commit(&mut self, upto: usize) -> Result<(), String> {
        if upto <= self.committed {
            return Ok(());
        }
        assert!(upto <= self.len && upto.is_multiple_of(GRAIN), "a commit of whole grains");
        let (start, len) = (self.at + self.committed as u64, upto - self.committed);
        // SAFETY: `start..start + len` is whole pages of this reservation (asserted), past what
        // is committed, which nothing reads or writes: committing it changes no byte.
        let done = unsafe { VirtualAlloc(start as *mut c_void, len, MEM_COMMIT, PAGE_READWRITE) };
        if done.is_null() {
            // alloc: error path
            return Err(format!(
                "VirtualAlloc of {len} bytes at {start:#x}: {}",
                std::io::Error::last_os_error()
            ));
        }
        self.committed = upto;
        Ok(())
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        // SAFETY: this reservation's own, released whole (size 0 with MEM_RELEASE), which no
        // view borrows any more (views borrow the reservation, and it is being dropped).
        unsafe { VirtualFree(self.at as *mut c_void, 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use super::{
        super::column::{Column, STEP},
        GetCurrentProcess, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MemoryBasicInformation, PAGE,
        PAGE_READWRITE, Regions, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
    };

    /// The regions read once give, for every address of an allocation, the protection
    /// `VirtualQuery` gives for it.
    #[test]
    fn regions_read_at_start_say_what_the_kernel_says() {
        const PAGE_READONLY: u32 = 0x02;
        let len = 3 * PAGE as usize;
        // SAFETY: a new allocation anywhere, which only this test uses.
        let at = unsafe {
            VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        };
        assert!(!at.is_null());
        let base = at as u64;
        let mut old = 0;
        let middle = (base + PAGE) as *mut c_void;
        // SAFETY: the middle page of the allocation above.
        assert_ne!(unsafe { VirtualProtect(middle, PAGE as usize, PAGE_READONLY, &mut old) }, 0);
        let regions = Regions::read(base).expect("read");
        let mut pieces = Vec::new();
        regions
            .each(&(base..base + len as u64), |piece, region| {
                pieces.push((piece, region.protect));
                Ok(())
            })
            .expect("committed");
        assert_eq!(
            pieces,
            vec![
                (base..base + PAGE, PAGE_READWRITE),
                (base + PAGE..base + 2 * PAGE, PAGE_READONLY),
                (base + 2 * PAGE..base + 3 * PAGE, PAGE_READWRITE),
            ]
        );
        assert!(regions.each(&(base..base + len as u64 + 1), |_, _| Ok(())).is_err());
        // SAFETY: the allocation above, released whole.
        unsafe { VirtualFree(at, 0, MEM_RELEASE) };
    }

    /// `PSAPI_WORKING_SET_EX_INFORMATION`: bit 0 of `attributes` is whether the page is in
    /// the working set.
    #[repr(C)]
    struct WorkingSetEx {
        address: *mut c_void,
        attributes: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32QueryWorkingSetEx(process: *mut c_void, info: *mut WorkingSetEx, len: u32) -> i32;
    }

    /// The state of the region at `at`, and how many bytes from `at` it runs.
    fn region(at: u64) -> (u32, u64) {
        let mut info = MemoryBasicInformation {
            base: std::ptr::null_mut(),
            allocation_base: std::ptr::null_mut(),
            allocation_protect: 0,
            partition: 0,
            size: 0,
            state: 0,
            protect: 0,
            kind: 0,
        };
        let len = size_of::<MemoryBasicInformation>();
        // SAFETY: `info` is a whole MEMORY_BASIC_INFORMATION.
        assert_eq!(unsafe { VirtualQuery(at as *const c_void, &mut info, len) }, len);
        (info.state, info.base as u64 + info.size as u64 - at)
    }

    /// How many pages of `at..at + len` are in the working set.
    fn resident(at: u64, len: usize) -> usize {
        let mut pages: Vec<WorkingSetEx> = (0..len as u64 / PAGE)
            .map(|it| WorkingSetEx { address: (at + it * PAGE) as *mut c_void, attributes: 0 })
            .collect();
        let size = (pages.len() * size_of::<WorkingSetEx>()) as u32;
        // SAFETY: it takes no pointer; the handle stands for this process.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: `pages` is `size` bytes of entries, each naming the page to ask about.
        let read = unsafe { K32QueryWorkingSetEx(process, pages.as_mut_ptr(), size) };
        assert_ne!(read, 0, "QueryWorkingSetEx: {}", std::io::Error::last_os_error());
        pages.iter().filter(|it| it.attributes & 1 != 0).count()
    }

    /// A column costs no memory for what it reserves, nor for what it commits, until a row is
    /// written: then the page the row is on.
    #[test]
    fn a_column_costs_memory_only_where_written() {
        let mut column = Column::<u64>::new("a test column", 1 << 20).expect("reserved");
        let (at, len) = column.reserved();
        assert_eq!(region(at), (MEM_RESERVE, len as u64), "reserved whole, nothing committed");
        assert_eq!(resident(at, len), 0);
        column.room(2 * STEP / 8).expect("committed");
        assert_eq!(region(at), (MEM_COMMIT, 2 * STEP as u64));
        assert_eq!(resident(at, len), 0, "committed, nothing written");
        column.push(7).expect("room");
        assert_eq!(resident(at, len), 1, "one row written");
        assert_eq!(column[0], 7);
    }
}
