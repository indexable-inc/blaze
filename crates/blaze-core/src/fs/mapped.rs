//! Files mapped read-only: the one way blaze reads a file in place (the store's packs, the
//! daemon's image through [`crate::Effect::Map`], the executables and libraries the splice and
//! the link read, the proof's outputs). Unsafe for `mmap` and `MapViewOfFile`; what it hands out
//! is a `&[u8]`, never a pointer.
//!
//! Mapped bytes come from disk and are untrusted (docs/v2/plan/design/unsafe.md): [`Mapped`]
//! gives bytes, which any bit pattern is, and nothing typed. A reader that views them as more (a
//! header read in place, a column of an image) checks their content hash and bounds-checks every
//! offset against the mapping's length first, as `blaze-image` checks its page groups.
//!
//! How a read stays in bounds. The length is captured once, at map time, from `fstat` of the
//! descriptor being mapped (so of the same inode, not a name that may since point elsewhere),
//! and the mapping is exactly that long (or, of a range, ends where the range does, which must
//! lie inside that length); [`Mapped::bytes`] is a slice of that length, so every index safe code
//! makes is checked against it, and nothing here reads past it whatever the file does later. A
//! file appended to after it is mapped changes nothing seen: the bytes past `len` are not in the
//! slice. A range starts at its offset rounded down to the page (64 KiB on Windows); the bytes
//! before it ([`Mapped::slack`]) are mapped but never in the slice.
//!
//! Another process may still write or cut the file (the store's packs and the image are
//! blaze's own files, but nothing in the kernel stops another writer). Writing one in place
//! breaks [`MapOptions::map`]'s contract: the bytes under a live `&[u8]` change, which a reader
//! sees as different bytes and its content-hash check refuses. Cutting one short makes a read
//! of a page past the new end fault: `SIGBUS` on Unix, `EXCEPTION_IN_PAGE_ERROR` on Windows
//! (which refuses to cut a file a view of which is open, so there only a failing disk does it).
//! blaze installs no handler for either: the process stops at the faulting read, so it never
//! goes on with bytes it did not read. Recovering from it (a `sigsetjmp` around each read, as
//! some databases do) would put a branch and a thread-local write on every read of a mapping
//! for a case the contract excludes.
//!
//! Why a mapping and not a read: a pack or an image is read a few pages at a time, in place,
//! shared by every reader; a read of the whole file costs a copy at memory bandwidth and the
//! memory to hold it.

use std::{fmt, fs::File, io, ops::Deref, path::Path, ptr::NonNull};

/// The file mapped read-only and shared, whole, as one [`Mapped`] that the last
/// [`crate::Mapping`] of it unmaps.
pub(crate) fn map(path: &Path) -> io::Result<crate::Mapping> {
    map_extent(path, crate::Extent::Whole)
}

/// `extent` of the file mapped read-only and shared, for [`crate::Effect::Map`], as one
/// [`Mapped`] that the last [`crate::Mapping`] of it unmaps.
pub(crate) fn map_extent(path: &Path, extent: crate::Extent) -> io::Result<crate::Mapping> {
    let file = File::open(path)?;
    // SAFETY: `Effect::Map`'s contract: while any `Mapping` of the file lives, the bytes below
    // the length it had when mapped are never written again and the file is never truncated
    // (it may be appended to, past every byte mapped); the store and the image replace a file
    // only by renaming another over it. Broken, a reader sees changed bytes (refused by its hash
    // check) or stops at a `SIGBUS` (the module doc).
    let mapped = unsafe {
        match extent {
            crate::Extent::Whole => MapOptions::new().map(&file),
            crate::Extent::Range { at, len } => MapOptions::new().map_range(&file, at, len),
        }
    }?;
    Ok(crate::Mapping::file(mapped))
}

/// From this length a mapping asks Linux for huge pages: one 2 MiB page each.
#[cfg(target_os = "linux")]
const HUGE: usize = 2 << 20;

/// How the system is told the mapping will be read. Only advice: the bytes read are the same.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Advice {
    #[default]
    Normal,
    /// Read once, front to back (a whole pack compressed, two files compared): read far ahead.
    Sequential,
    /// Read at scattered places (an index probed): no read-ahead.
    Random,
    /// Soon read whole: read ahead now, without waiting.
    WillNeed,
}

/// How a file is mapped: [`MapOptions::new`], changed by the builders, then
/// [`MapOptions::open`] or [`MapOptions::map`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MapOptions {
    populate: bool,
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "huge pages of files are Linux's")
    )]
    #[cfg_attr(
        not(target_os = "linux"),
        expect(blaze::unwired, reason = "wired on Linux, the one kernel with huge pages of files")
    )]
    huge: bool,
    advice: Advice,
}

impl Default for MapOptions {
    fn default() -> MapOptions {
        MapOptions::new()
    }
}

impl MapOptions {
    /// Huge pages where the system makes them for a file, nothing faulted in ahead, the
    /// system's own read-ahead.
    pub const fn new() -> MapOptions {
        MapOptions { populate: false, huge: true, advice: Advice::Normal }
    }

    /// Every page faulted in before the map returns (`MAP_POPULATE` on Linux; elsewhere the
    /// whole is read ahead, without waiting): for a file read whole at once, which then takes
    /// no page fault per page.
    pub const fn populate(self, populate: bool) -> MapOptions {
        MapOptions { populate, ..self }
    }

    /// Page-cache huge pages (`MADV_HUGEPAGE`) for a file of 2 MiB or more, on Linux where the
    /// file system makes them: the TLB then covers a pack in hundreds of entries rather than
    /// hundreds of thousands. Nothing elsewhere. On by default.
    pub const fn huge_pages(self, huge: bool) -> MapOptions {
        MapOptions { huge, ..self }
    }

    /// How the mapping will be read (`madvise` on Linux and macOS; on Windows only
    /// [`Advice::WillNeed`] does something, a `PrefetchVirtualMemory`).
    pub const fn advise(self, advice: Advice) -> MapOptions {
        MapOptions { advice, ..self }
    }

    /// The file at `path`, opened and mapped whole.
    ///
    /// # Safety
    ///
    /// As [`Mapped::map`].
    pub unsafe fn open(self, path: &Path) -> io::Result<Mapped> {
        let file = File::open(path)?;
        // SAFETY: the caller's contract, which `map` states.
        unsafe { self.map(&file) }
    }

    /// The whole of `file`, mapped read-only and shared with every other mapping of it; an
    /// empty file maps nothing. The handle may close after: the mapping keeps the file.
    ///
    /// # Safety
    ///
    /// While the [`Mapped`] lives, nothing (this process or another) writes the bytes below the
    /// length the file had when mapped, or truncates it; it may be appended to, past every byte
    /// mapped, and is otherwise replaced only by renaming another file over it, as the store and
    /// the image do. A write below that length would change bytes under a live `&[u8]`, and a
    /// truncate would fault on the next read of the pages cut. (Windows refuses to cut a file
    /// a view of which is open, but not to write it.)
    pub unsafe fn map(self, file: &File) -> io::Result<Mapped> {
        // `fstat` of the descriptor mapped: the one length every read is bounded by.
        let len = file.metadata()?.len();
        // SAFETY: the caller's contract, which `map_range`'s is.
        unsafe { self.map_range(file, 0, len) }
    }

    /// `len` bytes at `at` of `file`, mapped read-only and shared: mapped from `at` rounded down
    /// to the system's granularity ([`granularity`]: a page, 64 KiB on Windows), with
    /// [`Mapped::slack`] the bytes that took before `at`, and [`Mapped::bytes`] exactly the `len`
    /// asked, nothing past them. A range past the file's length (`fstat` of this descriptor)
    /// fails, naming the range and the length; an empty range maps nothing.
    ///
    /// # Safety
    ///
    /// As [`MapOptions::map`].
    pub unsafe fn map_range(self, file: &File, at: u64, len: u64) -> io::Result<Mapped> {
        let file_len = file.metadata()?.len();
        if at.checked_add(len).is_none_or(|end| end > file_len) {
            // alloc: an error path.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a mapping of {len} bytes at {at} runs past the file's end at {file_len}: a \
                     range is mapped only inside the file"
                ),
            ));
        }
        if len == 0 {
            return Ok(Mapped::EMPTY);
        }
        let granule = granularity() as u64;
        let from = at - at % granule;
        let slack = usize::try_from(at - from).expect("below the granularity, a usize");
        let len = usize::try_from(len).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "a range larger than memory")
        })?;
        let mapped = slack.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a range larger than memory")
        })?;
        // SAFETY: the caller's contract above; `mapped` is not 0, and `from` is a whole number of
        // the granularity with `from + mapped` inside the file.
        let base = unsafe { sys::map(file, from, mapped, self) }?;
        Ok(Mapped { base, slack, len })
    }
}

/// The granularity a mapping's offset is a whole number of: the page (`sysconf(_SC_PAGESIZE)`,
/// 16 KiB on Apple silicon, 4 KiB on most Linux), 64 KiB on Windows (the allocation
/// granularity `MapViewOfFile` asks), 1 on WASI, which reads the range into memory.
pub fn granularity() -> usize {
    static GRANULARITY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *GRANULARITY.get_or_init(sys::granularity)
}

/// A file's bytes, mapped read-only by [`Mapped::map`], [`MapOptions::map`] or
/// [`MapOptions::map_range`], unmapped when dropped. Derefs to its bytes.
pub struct Mapped {
    /// Where the mapping starts: `slack` bytes before the first byte shown.
    base: NonNull<u8>,
    /// Bytes mapped before the range asked: its offset's distance past the granularity.
    slack: usize,
    /// The bytes shown.
    len: usize,
}

// SAFETY: read-only pages that no one writes while they are mapped (`MapOptions::map`'s
// contract), so threads share them as they would a `&[u8]`; unmapping is `&mut self`'s, in drop.
unsafe impl Send for Mapped {}
// SAFETY: as above.
unsafe impl Sync for Mapped {}

impl Mapped {
    /// No bytes, and nothing to unmap: what an empty file maps.
    pub const EMPTY: Mapped = Mapped { base: NonNull::dangling(), slack: 0, len: 0 };

    /// The file at `path` mapped whole, with [`MapOptions::new`].
    ///
    /// # Safety
    ///
    /// As [`Mapped::map`].
    pub unsafe fn open(path: &Path) -> io::Result<Mapped> {
        // SAFETY: the caller's contract.
        unsafe { MapOptions::new().open(path) }
    }

    /// The whole of `file` mapped, with [`MapOptions::new`].
    ///
    /// # Safety
    ///
    /// While the [`Mapped`] lives, nothing writes the bytes below the length the file had when
    /// mapped or truncates it; it may be appended to ([`MapOptions::map`]).
    pub unsafe fn map(file: &File) -> io::Result<Mapped> {
        // SAFETY: the caller's contract.
        unsafe { MapOptions::new().map(file) }
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `slack + len` bytes mapped readable at `base` for as long as `self` lives (or
        // none, at a dangling, aligned `base` with no slack), never written or cut short
        // meanwhile by `MapOptions::map`'s contract; if they were, this slice would see a data
        // race or fault. The slice starts `slack` in, so it shows exactly the range asked.
        unsafe { std::slice::from_raw_parts(self.base.as_ptr().add(self.slack), self.len) }
    }

    /// The bytes mapped before the first one shown: how far the range's offset was past the
    /// granularity it was mapped from. 0 for a whole file.
    pub fn slack(&self) -> usize {
        self.slack
    }
}

impl Deref for Mapped {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes()
    }
}

impl AsRef<[u8]> for Mapped {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

impl fmt::Debug for Mapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mapped({} bytes)", self.len)
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: the mapping `sys::map` made, of `slack + len` bytes at `base`, unmapped
            // once; the bytes lent by `bytes` borrow `self`, so none outlives it.
            unsafe { sys::unmap(self.base, self.slack + self.len) };
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::{fs::File, io, os::fd::AsRawFd as _, ptr::NonNull};

    use super::MapOptions;

    /// The page: what `mmap`'s offset is a whole number of.
    pub(super) fn granularity() -> usize {
        // SAFETY: no pointers; `sysconf` reads a constant of the system.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page).ok().filter(|page| page.is_power_of_two()).unwrap_or_else(|| {
            panic!("sysconf(_SC_PAGESIZE) is {page}: no page size, so no mapping's offset")
        })
    }

    /// `len` bytes at `from` of `file`, mapped; where they start.
    ///
    /// # Safety
    ///
    /// As [`super::MapOptions::map`]; `len` is not 0, `from` is a whole number of
    /// [`granularity`], and `from + len` is inside the file.
    pub(super) unsafe fn map(
        file: &File,
        from: u64,
        len: usize,
        options: MapOptions,
    ) -> io::Result<NonNull<u8>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let populate = if options.populate { libc::MAP_POPULATE } else { 0 };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let populate = 0;
        let offset = libc::off_t::try_from(from).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "a mapping at an offset past off_t")
        })?;
        // SAFETY: a new shared read-only mapping of `len` bytes of an open descriptor at a
        // page-aligned offset inside it, at an address the kernel picks, so it overlaps nothing
        // the program holds; the result is checked against `MAP_FAILED` below. The descriptor
        // may close after: the mapping keeps the file.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED | populate,
                file.as_raw_fd(),
                offset,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        advise(at, len, options);
        // A mapping with no address asked for is never at 0 (`vm.mmap_min_addr`).
        Ok(NonNull::new(at.cast::<u8>()).expect("mmap mapped a file at address 0"))
    }

    /// The mapping's advice, which is only advice: a call the system refuses changes nothing
    /// of the bytes read, so its result is not looked at.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(unused_variables))]
    fn advise(at: *mut libc::c_void, len: usize, options: MapOptions) {
        #[cfg(target_os = "linux")]
        if options.huge && len >= super::HUGE {
            // SAFETY: the whole mapping `map` just made; the advice changes how its pages are
            // backed, never their bytes.
            _ = unsafe { libc::madvise(at, len, libc::MADV_HUGEPAGE) };
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use super::Advice;
            let advice = match options.advice {
                Advice::Normal => None,
                Advice::Sequential => Some(libc::MADV_SEQUENTIAL),
                Advice::Random => Some(libc::MADV_RANDOM),
                Advice::WillNeed => Some(libc::MADV_WILLNEED),
            };
            if let Some(advice) = advice {
                // SAFETY: as above.
                _ = unsafe { libc::madvise(at, len, advice) };
            }
            // macOS has no `MAP_POPULATE`: the whole is read ahead instead.
            #[cfg(target_os = "macos")]
            if options.populate && options.advice != Advice::WillNeed {
                // SAFETY: as above.
                _ = unsafe { libc::madvise(at, len, libc::MADV_WILLNEED) };
            }
        }
    }

    /// # Safety
    ///
    /// `at` and `len` are a mapping `map` made, unmapped once, that nothing borrows.
    pub(super) unsafe fn unmap(at: NonNull<u8>, len: usize) {
        // SAFETY: the caller's contract.
        unsafe { libc::munmap(at.as_ptr().cast(), len) };
    }
}

#[cfg(windows)]
mod sys {
    use std::{
        fs::File,
        io,
        os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
        ptr::NonNull,
    };

    use windows_sys::Win32::System::{
        Memory::{
            CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            PAGE_READONLY, PrefetchVirtualMemory, UnmapViewOfFile, WIN32_MEMORY_RANGE_ENTRY,
        },
        Threading::GetCurrentProcess,
    };

    use super::{Advice, MapOptions};

    /// The allocation granularity `MapViewOfFile`'s offset is a whole number of: 64 KiB on
    /// every Windows blaze runs on (`SYSTEM_INFO::dwAllocationGranularity`).
    pub(super) fn granularity() -> usize {
        64 << 10
    }

    /// `len` bytes at `from` of `file`, mapped; where they start.
    ///
    /// # Safety
    ///
    /// As [`super::MapOptions::map`]; `len` is not 0 (an empty file has no section), `from` is
    /// a whole number of [`granularity`], and `from + len` is inside the file.
    pub(super) unsafe fn map(
        file: &File,
        from: u64,
        len: usize,
        options: MapOptions,
    ) -> io::Result<NonNull<u8>> {
        // The section ends where the range does, the end every read is bounded by: Windows
        // refuses a read-only section longer than the file, so one cut short since `fstat`
        // fails here rather than leaving the slice longer than the view.
        let size = from + len as u64;
        // SAFETY: an open handle; the section is `size` bytes of the file, read-only, unnamed,
        // with no security attributes. The result is checked below.
        let section = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READONLY,
                (size >> 32) as u32,
                size as u32,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the section just made, a handle of its own that nothing else owns.
        let section = unsafe { OwnedHandle::from_raw_handle(section) };
        // SAFETY: the section just made; `len` bytes of it from `from`, a whole number of the
        // allocation granularity, to its end, read-only. The result is checked below.
        let view = unsafe {
            MapViewOfFile(
                section.as_raw_handle(),
                FILE_MAP_READ,
                (from >> 32) as u32,
                from as u32,
                len,
            )
        };
        let Some(at) = NonNull::new(view.Value.cast::<u8>()) else {
            return Err(io::Error::last_os_error());
        };
        // The view keeps the section and the file open after their handles close.
        drop(section);
        if options.populate || options.advice == Advice::WillNeed {
            let range =
                WIN32_MEMORY_RANGE_ENTRY { VirtualAddress: at.as_ptr().cast(), NumberOfBytes: len };
            // SAFETY: a pseudo-handle of this process, which needs no closing.
            let process = unsafe { GetCurrentProcess() };
            // SAFETY: one range, of the view just made; a prefetch reads its pages in, never
            // changes them. Only advice: its result is not looked at.
            _ = unsafe { PrefetchVirtualMemory(process, 1, &range, 0) };
        }
        Ok(at)
    }

    /// # Safety
    ///
    /// `at` is a view `map` made, unmapped once, that nothing borrows.
    pub(super) unsafe fn unmap(at: NonNull<u8>, _len: usize) {
        let view = MEMORY_MAPPED_VIEW_ADDRESS { Value: at.as_ptr().cast() };
        // SAFETY: the caller's contract.
        unsafe { UnmapViewOfFile(view) };
    }
}

/// WASI has no mapping, so a "mapping" there is the file read whole, once, into memory of its
/// own, page-aligned as a mapping is: the bytes a mapping would show when it was made, which no
/// later write to the file changes. Every read of them is then a read of memory; the options
/// say nothing to a read.
#[cfg(target_os = "wasi")]
mod sys {
    use std::{alloc::Layout, fs::File, io, os::wasi::fs::FileExt as _, ptr::NonNull};

    use super::MapOptions;

    /// A page, as elsewhere: what a reader that views the bytes in place may count on.
    const ALIGN: usize = 4096;

    /// A range is read from its own offset, so nothing is read before it.
    pub(super) fn granularity() -> usize {
        1
    }

    /// `len` bytes at `from` of `file`, read into memory of their own; where they start.
    ///
    /// # Safety
    ///
    /// `len` is not 0.
    pub(super) unsafe fn map(
        file: &File,
        from: u64,
        len: usize,
        _: MapOptions,
    ) -> io::Result<NonNull<u8>> {
        let layout = Layout::from_size_align(len, ALIGN).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "a file larger than memory")
        })?;
        // SAFETY: `layout` is not of size 0, by the caller. Zeroed, so the bytes are
        // initialized before they are read into.
        // alloc: the file's bytes, WASI's stand-in for its mapping; freed by `unmap`.
        let at = NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).ok_or_else(|| {
            // alloc: an error path.
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!("{len} bytes to read the file into: WASI maps no file"),
            )
        })?;
        // SAFETY: `len` initialized bytes at `at`, just allocated, this function's alone.
        let bytes = unsafe { std::slice::from_raw_parts_mut(at.as_ptr(), len) };
        match file.read_exact_at(bytes, from) {
            Ok(()) => Ok(at),
            Err(error) => {
                // SAFETY: allocated above with `layout`, and lent to nothing now.
                unsafe { std::alloc::dealloc(at.as_ptr(), layout) };
                // alloc: an error path.
                Err(io::Error::new(
                    error.kind(),
                    format!("reading its {len} bytes whole (WASI maps no file): {error}"),
                ))
            }
        }
    }

    /// # Safety
    ///
    /// `at` and `len` are of one `map`, freed once, that nothing borrows.
    pub(super) unsafe fn unmap(at: NonNull<u8>, len: usize) {
        // SAFETY: the caller's contract: `map` allocated `len` bytes at `at` with this layout,
        // which `Layout::from_size_align` accepted then.
        unsafe { std::alloc::dealloc(at.as_ptr(), Layout::from_size_align_unchecked(len, ALIGN)) };
    }
}

#[cfg(not(any(unix, windows, target_os = "wasi")))]
mod sys {
    use std::{fs::File, io, ptr::NonNull};

    use super::MapOptions;

    pub(super) fn granularity() -> usize {
        1
    }

    /// # Safety
    ///
    /// None: nothing is mapped.
    pub(super) unsafe fn map(
        _: &File,
        _: u64,
        _: usize,
        _: MapOptions,
    ) -> io::Result<NonNull<u8>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this system has no file mapping blaze uses",
        ))
    }

    /// # Safety
    ///
    /// None: `map` never maps.
    pub(super) unsafe fn unmap(_: NonNull<u8>, _: usize) {}
}

#[cfg(test)]
mod tests;
