//! What Linux and macOS share: finding the descriptor with `dlsym`, walking a stack with
//! `_Unwind_Backtrace`, the unwinder's `__register_frame`, and address space reserved with
//! `mmap` and committed with `mprotect` ([`Reserved`]).

#![allow(unsafe_code, reason = "the loader's and the kernel's calls both Unix platforms share")]

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ops::Range,
};

use super::{hook::FRAMES, image};
use crate::patch::{DESCRIPTOR, Descriptor};

#[repr(C)]
#[allow(dead_code)]
struct DlInfo {
    fname: *const c_char,
    fbase: *mut c_void,
    sname: *const c_char,
    saddr: *mut c_void,
}

unsafe extern "C" {
    fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int;
    fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn _Unwind_Backtrace(
        trace: extern "C" fn(*mut c_void, *mut c_void) -> c_int,
        data: *mut c_void,
    ) -> c_int;
    fn _Unwind_GetIP(context: *mut c_void) -> usize;
    pub(super) fn __register_frame(frame: *const u8);
    pub(super) fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    pub(super) fn munmap(addr: *mut c_void, len: usize) -> c_int;
    pub(super) fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
}

const PROT_NONE: c_int = 0;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_PRIVATE: c_int = 2;
#[cfg(target_os = "linux")]
const MAP_ANONYMOUS: c_int = 0x20;
#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: c_int = 0x1000;
#[cfg(target_os = "linux")]
const MAP_NORESERVE: c_int = 0x4000;
#[cfg(target_os = "macos")]
const MAP_NORESERVE: c_int = 0x40;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

const RTLD_LAZY: c_int = 1;
#[cfg(target_os = "linux")]
const RTLD_NOLOAD: c_int = 4;
#[cfg(target_os = "macos")]
const RTLD_NOLOAD: c_int = 0x10;
const URC_NO_REASON: c_int = 0;
const URC_END_OF_STACK: c_int = 5;

/// The image that has `address`: where it starts and its path as the loader has it.
fn image_of(address: *const c_void) -> Option<(u64, *const c_char)> {
    let mut info = DlInfo {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a whole `Dl_info` for dladdr to fill.
    (unsafe { dladdr(address, &mut info) } != 0).then_some((info.fbase as u64, info.fname))
}

/// Where the image that has `address` starts, for the platforms' check that a patch's data
/// is in the image it was linked for.
pub(super) fn image_base(address: u64) -> Option<u64> {
    image_of(address as *const c_void).map(|it| it.0)
}

/// Checks that the first and the last byte of `range` are in the image at `base`. The loader
/// maps an image as one run of addresses; a gap within it is refused by the platform's walk
/// of the mappings (unmapped, or mapped with no access).
pub(super) fn in_image(range: &Range<u64>, base: u64) -> Result<(), String> {
    let last = range.end.saturating_sub(1).max(range.start);
    if image_base(range.start) == Some(base) && image_base(last) == Some(base) {
        Ok(())
    } else {
        Err(outside(range, base))
    }
}

fn outside(range: &Range<u64>, base: u64) -> String {
    // alloc: error path
    format!("data at {:#x}..{:#x}, outside the image at {base:#x}", range.start, range.end)
}

/// The address ranges the image the loader put at `base` is mapped in, sorted: those the
/// loader's `dladdr` finds an address of the image in (ELF `PT_LOAD` segments, Mach-O
/// segments but `__PAGEZERO`), read once at start. [`Segments::check`] is [`in_image`] by
/// binary search, where `dladdr` walks the loader's list of images and their symbols for
/// each address.
pub(super) struct Segments {
    base: u64,
    ranges: Vec<Range<u64>>,
}

impl Segments {
    fn new(base: u64, mut ranges: Vec<Range<u64>>) -> Result<Segments, String> {
        ranges.retain(|it| !it.is_empty());
        ranges.sort_unstable_by_key(|it| it.start);
        if ranges.is_empty() {
            // alloc: error path
            return Err(format!("the image at {base:#x} has no segments"));
        }
        Ok(Segments { base, ranges })
    }

    /// Whether `at` is in a segment.
    fn has(&self, at: u64) -> bool {
        let next = self.ranges.partition_point(|it| it.end <= at);
        self.ranges.get(next).is_some_and(|it| it.start <= at)
    }

    /// Checks that the first and the last byte of `range` are in the image, as [`in_image`].
    pub(super) fn check(&self, range: &Range<u64>) -> Result<(), String> {
        let last = range.end.saturating_sub(1).max(range.start);
        if self.has(range.start) && self.has(last) {
            Ok(())
        } else {
            Err(outside(range, self.base))
        }
    }

    /// [`Segments::check`] of ranges sorted by address, one call each in order: `next` starts
    /// at 0 and only moves forward, so checking `n` ranges against `s` segments is `n + s`
    /// steps where a binary search for each is `n log s` with a mispredicted branch or two.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn check_from(&self, next: &mut usize, range: &Range<u64>) -> Result<(), String> {
        let last = range.end.saturating_sub(1).max(range.start);
        let mut has = |at: u64| {
            while self.ranges.get(*next).is_some_and(|it| it.end <= at) {
                *next += 1;
            }
            self.ranges.get(*next).is_some_and(|it| it.start <= at)
        };
        if has(range.start) && has(last) { Ok(()) } else { Err(outside(range, self.base)) }
    }

    /// Where the image is loaded.
    pub(super) fn base(&self) -> u64 {
        self.base
    }

    /// From the start of the first segment to the end of the last, in memory.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn hull(&self) -> Range<u64> {
        let first = self.ranges.first().map_or(0, |it| it.start);
        first..self.ranges.last().map_or(first, |it| it.end)
    }

    /// The segments of the image that has `address`.
    #[cfg(target_os = "linux")]
    pub(super) fn of(address: u64) -> Result<Segments, String> {
        /// The first fields of `dl_phdr_info`, which the loader passes by pointer.
        #[repr(C)]
        #[allow(dead_code)]
        struct Info {
            addr: u64,
            name: *const c_char,
            phdr: *const Phdr,
            phnum: u16,
        }
        /// `Elf64_Phdr`.
        #[repr(C)]
        #[allow(dead_code)]
        struct Phdr {
            kind: u32,
            flags: u32,
            offset: u64,
            vaddr: u64,
            paddr: u64,
            filesz: u64,
            memsz: u64,
            align: u64,
        }
        const PT_LOAD: u32 = 1;
        unsafe extern "C" {
            fn dl_iterate_phdr(
                each: extern "C" fn(*mut Info, usize, *mut c_void) -> c_int,
                data: *mut c_void,
            ) -> c_int;
        }
        struct Found {
            address: u64,
            ranges: Vec<Range<u64>>,
        }
        extern "C" fn each(info: *mut Info, _: usize, data: *mut c_void) -> c_int {
            // SAFETY: `data` is the `Found` `of` passed, borrowed for the walk.
            let found = unsafe { &mut *(data as *mut Found) };
            // SAFETY: the loader's `dl_phdr_info` of one object, valid during this call; its
            // first four fields are `Info`'s.
            let info = unsafe { &*info };
            if info.phdr.is_null() {
                return 0;
            }
            // SAFETY: `phdr` is the object's array of `phnum` program headers, mapped as long
            // as the object is loaded, which it is during this call.
            let headers = unsafe { std::slice::from_raw_parts(info.phdr, usize::from(info.phnum)) };
            let loads = headers.iter().filter(|it| it.kind == PT_LOAD).map(|it| {
                let start = info.addr.wrapping_add(it.vaddr);
                start..start.saturating_add(it.memsz)
            });
            // clone: an iterator over the program headers, walked twice; it copies no element
            if !loads.clone().any(|it| it.contains(&found.address)) {
                return 0;
            }
            found.ranges.extend(loads);
            1
        }
        let base = image_base(address)
            // alloc: error path
            .ok_or_else(|| format!("no image has the address {address:#x}"))?;
        let mut found = Found { address, ranges: Vec::with_capacity(16) };
        // SAFETY: `each` only reads the loader's headers and pushes to the `Found` it is given.
        unsafe { dl_iterate_phdr(each, &mut found as *mut Found as *mut c_void) };
        Segments::new(base, found.ranges)
    }

    /// The segments of the image that has `address`, from its Mach-O header.
    #[cfg(target_os = "macos")]
    pub(super) fn of(address: u64) -> Result<Segments, String> {
        const MH_MAGIC_64: u32 = 0xfeed_facf;
        const LC_SEGMENT_64: u32 = 0x19;
        const HEADER: usize = 32;
        const SEGMENT: usize = 72;
        let base = image_base(address)
            // alloc: error path
            .ok_or_else(|| format!("no image has the address {address:#x}"))?;
        let u32_at = |bytes: &[u8], at: usize| {
            u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
        };
        let u64_at = |bytes: &[u8], at: usize| {
            u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
        };
        // SAFETY: `base` is where dyld mapped the image's Mach-O header (dladdr's `dli_fbase`),
        // the start of its `__TEXT`, read-only for as long as the image is loaded.
        let header = unsafe { image::view(base, HEADER) };
        if u32_at(header, 0) != MH_MAGIC_64 {
            // alloc: error path
            return Err(format!("the image at {base:#x} is not a 64-bit Mach-O image"));
        }
        let (count, len) = (u32_at(header, 16), u32_at(header, 20) as usize);
        // SAFETY: the load commands follow the header in the same mapped `__TEXT`, `len`
        // bytes by the header, which dyld checked when it loaded the image.
        let commands = unsafe { image::view(base + HEADER as u64, len) };
        // alloc: setup, the image's segments, read once at start
        let (mut at, mut segments, mut slide) = (0usize, Vec::new(), None);
        for _ in 0..count {
            if at + 8 > commands.len() {
                break;
            }
            let (kind, size) = (u32_at(commands, at), u32_at(commands, at + 4) as usize);
            if size < 8 || at + size > commands.len() {
                // alloc: error path
                return Err(format!("the image at {base:#x} has a load command past its end"));
            }
            if kind == LC_SEGMENT_64 && size >= SEGMENT {
                let (vmaddr, vmsize) = (u64_at(commands, at + 24), u64_at(commands, at + 32));
                let (fileoff, filesize) = (u64_at(commands, at + 40), u64_at(commands, at + 48));
                let initprot = u32_at(commands, at + 60);
                if fileoff == 0 && filesize != 0 {
                    slide = Some(base.wrapping_sub(vmaddr));
                }
                if initprot != 0 {
                    segments.push((vmaddr, vmsize));
                }
            }
            at += size;
        }
        // alloc: error path
        let slide = slide.ok_or_else(|| format!("the image at {base:#x} maps no header"))?;
        let ranges = segments
            .into_iter()
            .map(|(vmaddr, vmsize)| {
                let start = vmaddr.wrapping_add(slide);
                start..start.saturating_add(vmsize)
            })
            // alloc: setup, as `segments`
            .collect();
        Segments::new(base, ranges)
    }
}

/// The descriptor of the image `base` from `handle`, if that is where `handle` finds it.
fn lookup(handle: *mut c_void, name: &CStr, base: u64) -> Option<u64> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: the handle is open, the name ends in a zero.
    let found = unsafe { dlsym(handle, name.as_ptr()) };
    (!found.is_null() && image_of(found)?.0 == base).then_some(found as u64)
}

/// The image's bytes at `range`, checked to be in the image at `base`; the caller checks
/// they are readable (the platform's walk of its mappings) or knows (the descriptor, which
/// the loader found by name).
pub(super) fn bytes(range: &Range<u64>, base: u64) -> Result<&'static [u8], String> {
    in_image(range, base)?;
    // SAFETY: `range` is in the image at `base` (checked above), which stays loaded as long as
    // this code, the image's own, runs; what is read is data the linker wrote, which no patch
    // writes over (the switch refuses data over the descriptor's cell and table, and the
    // descriptor and the directory name are in read-only data).
    Ok(unsafe { image::view(range.start, (range.end - range.start) as usize) })
}

/// Where the descriptor of the image this code is in is, in memory, and its bytes.
pub(super) fn descriptor() -> Result<(u64, &'static [u8]), String> {
    let at = find()?;
    // alloc: error path
    let base = image_base(at).ok_or_else(|| format!("no image has {DESCRIPTOR} at {at:#x}"))?;
    let end =
        at.checked_add(Descriptor::SIZE as u64).ok_or("a descriptor past the end of memory")?;
    Ok((at, bytes(&(at..end), base)?))
}

fn find() -> Result<u64, String> {
    let name = CString::new(DESCRIPTOR).map_err(|_| "a descriptor name with a zero")?;
    let own = descriptor as fn() -> Result<(u64, &'static [u8]), String> as *const c_void;
    let (base, path) = image_of(own).ok_or("the loader knows no image of blaze-live's own code")?;
    // SAFETY: a null path asks for the program's handle, which is never unloaded.
    let program = unsafe { dlopen(std::ptr::null(), RTLD_LAZY) };
    if let Some(found) = lookup(program, &name, base) {
        return Ok(found);
    }
    // A shared library that links blaze-live is found by its own path.
    let path = (!path.is_null())
        .then(|| {
            // SAFETY: the loader's path of a loaded image ends in a zero and lives as long as
            // it does.
            unsafe { CStr::from_ptr(path) }
        })
        .filter(|it| !it.is_empty());
    if let Some(path) = path {
        // SAFETY: NOLOAD opens nothing new; the handle is closed below.
        let handle = unsafe { dlopen(path.as_ptr(), RTLD_LAZY | RTLD_NOLOAD) };
        let found = lookup(handle, &name, base);
        if !handle.is_null() {
            // SAFETY: opened above; the image stays loaded, as it was before.
            unsafe { dlclose(handle) };
        }
        if let Some(found) = found {
            return Ok(found);
        }
    }
    let path =
        // alloc: error path
        path.map_or_else(|| "the program".to_owned(), |it| it.to_string_lossy().into_owned());
    // alloc: error path
    Err(format!("{path} exports no {DESCRIPTOR}: blaze-link links it for live patches"))
}

/// Pushes the code address of each frame of this thread's stack, newest first.
pub(super) fn backtrace(pcs: &mut Vec<u64>) {
    extern "C" fn each(context: *mut c_void, data: *mut c_void) -> c_int {
        // SAFETY: `data` is the vector `backtrace` passed, borrowed for the walk.
        let pcs = unsafe { &mut *(data as *mut Vec<u64>) };
        // SAFETY: the unwinder's context of this step of the walk.
        let ip = unsafe { _Unwind_GetIP(context) } as u64;
        if ip != 0 {
            pcs.push(ip - 1);
        }
        if pcs.len() >= FRAMES { URC_END_OF_STACK } else { URC_NO_REASON }
    }
    // SAFETY: `each` only pushes to the vector it is given.
    unsafe { _Unwind_Backtrace(each, pcs as *mut Vec<u64> as *mut c_void) };
}

/// What the table cell at `cell` holds, by one acquire load.
pub(super) fn load_cell(cell: u64) -> u64 {
    // SAFETY: the cell is the descriptor's, 8-aligned (`Layout::new` checked), checked to be
    // in the image by `Switch::new` before the first load, and only ever accessed atomically.
    unsafe { image::cell(cell) }.load(std::sync::atomic::Ordering::Acquire)
}

/// The loader's table at `at`, `slots` addresses.
pub(super) fn loaders(at: u64, slots: u64) -> &'static [std::sync::atomic::AtomicU64] {
    // SAFETY: `Switch::new` checked `at..at + slots * 8` is in the image and `at` 8-aligned
    // before the first call; it is the image's writable data (the linker's `Class::Table`),
    // which only `publish`'s atomic stores write (the switch refuses data over it).
    unsafe { image::words(at, slots as usize) }
}

/// Checks that `range` is in the image that has `near` (the descriptor's address).
pub(super) fn in_image_of(range: &Range<u64>, near: u64) -> Result<(), String> {
    // alloc: error path
    let base = image_base(near).ok_or_else(|| format!("no image has {near:#x}"))?;
    in_image(range, base)
}

/// What a reservation's length and commits are multiples of: the largest page of the
/// platforms (64 KiB, AArch64 Linux built for it), so every commit is whole pages.
pub(super) const GRAIN: usize = 64 << 10;

/// Address space reserved with no access and no memory behind it (`PROT_NONE`,
/// `MAP_NORESERVE`), committed read and write from its start as it grows, never moved, and
/// unmapped when dropped. A committed page costs memory only once written.
///
/// Invariant, which [`image::rows`] relies on: `at..at + committed` is mapped read and write,
/// zero where never written, and no one else's; `at` is page-aligned.
pub(super) struct Reserved {
    at: u64,
    len: usize,
    committed: usize,
}

impl Reserved {
    /// Reserves `len` bytes, a nonzero multiple of [`GRAIN`].
    pub(super) fn new(len: usize) -> Result<Reserved, String> {
        assert!(len != 0 && len.is_multiple_of(GRAIN), "a reservation of whole grains");
        let flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE;
        // SAFETY: a new private mapping anywhere, with no access: it changes no memory there is.
        let at = unsafe { mmap(std::ptr::null_mut(), len, PROT_NONE, flags, -1, 0) };
        if at == MAP_FAILED {
            // alloc: error path, once at start
            return Err(format!(
                "mmap of {len} bytes of address space: {}",
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
    /// `mprotect` of what was not committed yet.
    pub(super) fn commit(&mut self, upto: usize) -> Result<(), String> {
        if upto <= self.committed {
            return Ok(());
        }
        assert!(upto <= self.len && upto.is_multiple_of(GRAIN), "a commit of whole grains");
        let (start, len) = (self.at + self.committed as u64, upto - self.committed);
        // SAFETY: `start..start + len` is whole pages of this reservation (asserted), past what
        // is committed, which nothing reads or writes: giving it access changes no byte.
        if unsafe { mprotect(start as *mut c_void, len, PROT_READ | PROT_WRITE) } != 0 {
            // alloc: error path
            return Err(format!(
                "mprotect of {len} bytes at {start:#x}: {}",
                std::io::Error::last_os_error()
            ));
        }
        self.committed = upto;
        Ok(())
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        // SAFETY: this reservation's own mapping, which no view borrows any more (views borrow
        // the reservation, and it is being dropped).
        unsafe { munmap(self.at as *mut c_void, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_int, c_void};

    use super::super::column::{Column, STEP};
    use super::Segments;

    /// Checking ranges in order with a cursor that moves forward refuses and accepts what
    /// `check`'s searches do.
    #[test]
    fn a_forward_check_of_the_segments_is_a_search_for_each() {
        let segments = Segments::new(0x1000, vec![0x5000..0x6000, 0x1000..0x3000, 0x3000..0x4000])
            .expect("some");
        let ranges = [
            0x800..0x1008,
            0x1000..0x1008,
            0x2ff8..0x3008,
            0x3ff8..0x4008,
            0x4100..0x4108,
            0x5000..0x5ff8,
            0x5ff8..0x6008,
        ];
        let mut next = 0;
        for range in &ranges {
            let searched = segments.check(range).is_ok();
            assert_eq!(segments.check_from(&mut next, range).is_ok(), searched, "{range:x?}");
        }
    }

    unsafe extern "C" {
        fn mincore(addr: *mut c_void, len: usize, vec: *mut u8) -> c_int;
    }

    /// How many pages of `at..at + len` are resident.
    fn resident(at: u64, len: usize) -> usize {
        // One byte per 4 KiB is room for a byte per page of any page size.
        let mut pages = vec![0u8; len / 4096];
        // SAFETY: `at..at + len` is one reservation, mapped; `pages` has a byte per page.
        let read = unsafe { mincore(at as *mut c_void, len, pages.as_mut_ptr()) };
        assert_eq!(read, 0, "mincore: {}", std::io::Error::last_os_error());
        pages.iter().filter(|it| **it & 1 != 0).count()
    }

    /// A column costs no memory for what it reserves, nor for what it commits, until a row is
    /// written: then the page the row is on.
    #[test]
    fn a_column_costs_memory_only_where_written() {
        let mut column = Column::<u64>::new("a test column", 1 << 20).expect("reserved");
        let (at, len) = column.reserved();
        assert_eq!(len, 8 << 20);
        assert_eq!(resident(at, len), 0, "reserved, nothing committed");
        column.room(2 * STEP / 8).expect("committed");
        assert_eq!(column.committed(), 2 * STEP);
        assert_eq!(resident(at, len), 0, "committed, nothing written");
        column.push(7).expect("room");
        assert_eq!(resident(at, len), 1, "one row written");
        assert_eq!(column[0], 7);
    }
}
