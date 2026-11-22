//! Memory maps: `mmap` and what changes a mapping (`munmap`, `mprotect`, `madvise`, `msync`,
//! `mincore`), with the protection, map and advice sets. On Linux the typed calls are
//! [`crate::linux`]'s own (raw system calls); on macOS they are libSystem's, with macOS's
//! numbers under the same names. `MAP_FAILED` has no item: a failed map is the `Err` of
//! [`mmap`]. Linux's `msync` and `mincore` are `linux::fdctl`'s raw system calls.

#[cfg(target_os = "macos")]
use core::{
    ffi::{c_int, c_void},
    ptr::NonNull,
};

#[cfg(target_os = "macos")]
use crate::Errno;

// ---- Linux: the raw-system-call layer's own items. ----

/// replaces libc::mmap, libc::MAP_FAILED (the `Err`). Linux: [`crate::linux::mmap`].
#[cfg(target_os = "linux")]
pub use crate::linux::mmap;
/// replaces libc::munmap. Linux: [`crate::linux::munmap`].
#[cfg(target_os = "linux")]
pub use crate::linux::munmap;
/// replaces libc::mprotect. Linux: [`crate::linux::mprotect`].
#[cfg(target_os = "linux")]
pub use crate::linux::mprotect;
/// replaces libc::madvise. Linux: [`crate::linux::madvise`].
#[cfg(target_os = "linux")]
pub use crate::linux::madvise;
/// replaces libc::PROT_NONE (`Prot::NONE`), libc::PROT_READ, libc::PROT_WRITE, libc::PROT_EXEC.
/// Linux: [`crate::linux::Prot`].
#[cfg(target_os = "linux")]
pub use crate::linux::Prot;
/// replaces libc::MAP_SHARED, libc::MAP_PRIVATE, libc::MAP_FIXED, libc::MAP_ANONYMOUS,
/// libc::MAP_ANON (both `MapFlags::ANONYMOUS`), libc::MAP_NORESERVE (`NO_RESERVE`),
/// libc::MAP_POPULATE, libc::MAP_HUGETLB and libc::MAP_HUGE_2MB (`HUGETLB`, `HUGE_2MB`: the
/// two members linux/mem.rs lacks, an edit of this lane's report). Linux:
/// [`crate::linux::MapFlags`].
/// replaces libc::MAP_POPULATE (blaze_sys::posix::io::MapFlags::POPULATE; Linux only)
/// replaces libc::MAP_STACK (blaze_sys::posix::io::MapFlags::STACK; Linux only)
/// replaces libc::MAP_FIXED_NOREPLACE (blaze_sys::posix::io::MapFlags::FIXED_NO_REPLACE; Linux only)
/// replaces libc::MAP_JIT (blaze_sys::posix::io::MapFlags::JIT; macOS only)
#[cfg(target_os = "linux")]
pub use crate::linux::MapFlags;
/// replaces libc::MADV_DONTNEED, libc::MADV_WILLNEED, libc::MADV_SEQUENTIAL,
/// libc::MADV_RANDOM, libc::MADV_HUGEPAGE, libc::MADV_NOHUGEPAGE, libc::MADV_DONTFORK
/// (`Advice::DontFork`: the variant linux/mem.rs lacks, an edit of this lane's report).
/// Linux: [`crate::linux::Advice`].
/// replaces libc::MADV_HUGEPAGE (blaze_sys::posix::io::Advice::HugePage; Linux only)
/// replaces libc::MADV_NOHUGEPAGE (blaze_sys::posix::io::Advice::NoHugePage; Linux only)
/// replaces libc::MADV_DONTFORK (blaze_sys::posix::io::Advice::DontFork; Linux only)
/// replaces libc::MADV_FREE (blaze_sys::posix::io::Advice::Free; both systems, each its own number)
#[cfg(target_os = "linux")]
pub use crate::linux::Advice;

#[cfg(target_os = "macos")]
pub use on_macos::*;

/// macOS's map calls, over libSystem, with the Linux layer's signatures.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) mod on_macos {
    use core::{
        ffi::{c_int, c_void},
        ptr::NonNull,
    };

    use crate::{Errno, Fd, posix::types::off_t};

    /// libSystem's map calls the cold table does not hold (sys/mman.h).
    pub(in crate::posix::io) mod sys_mem {
        use core::ffi::{c_int, c_void};

        use crate::posix::types::off_t;

        unsafe extern "C" {
            /// sys/mman.h: `void *mmap(void *addr, size_t len, int prot, int flags, int fd, off_t offset)`.
            pub(in crate::posix::io) fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: off_t) -> *mut c_void;
            /// sys/mman.h: `int munmap(void *addr, size_t len)`.
            pub(in crate::posix::io) fn munmap(addr: *mut c_void, len: usize) -> c_int;
            /// sys/mman.h: `int mprotect(void *addr, size_t len, int prot)`.
            pub(in crate::posix::io) fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
            /// sys/mman.h: `int msync(void *addr, size_t len, int flags)`.
            pub(in crate::posix::io) fn msync(addr: *mut c_void, len: usize, flags: c_int) -> c_int;
        }
    }

    flag_set! {
        /// What a mapping's pages may be used for (`PROT_*`, sys/mman.h); [`Prot::NONE`]
        /// reserves. replaces libc::PROT_NONE.
        Prot: u32 {
            /// `PROT_READ`. replaces libc::PROT_READ.
            READ = 1;
            /// `PROT_WRITE`. replaces libc::PROT_WRITE.
            WRITE = 2;
            /// `PROT_EXEC`. replaces libc::PROT_EXEC.
            EXEC = 4;
        }
    }

    flag_set! {
        /// `mmap`'s flags (sys/mman.h, macOS's numbers).
        MapFlags: u32 {
            /// `MAP_SHARED`. replaces libc::MAP_SHARED.
            SHARED = 0x0001;
            /// `MAP_PRIVATE`. replaces libc::MAP_PRIVATE.
            PRIVATE = 0x0002;
            /// `MAP_FIXED`: at the address given, replacing what is mapped there. replaces
            /// libc::MAP_FIXED.
            FIXED = 0x0010;
            /// `MAP_NORESERVE`. replaces libc::MAP_NORESERVE.
            NO_RESERVE = 0x0040;
            /// `MAP_JIT`: pages that may be made executable under the hardened runtime.
            JIT = 0x0800;
            /// `MAP_ANON` (`MAP_ANONYMOUS`): zero pages, of no file. replaces libc::MAP_ANON,
            /// libc::MAP_ANONYMOUS.
            ANONYMOUS = 0x1000;
        }
    }

    /// What `madvise` tells the kernel about a range (sys/mman.h, macOS's numbers).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Advice {
        /// `MADV_NORMAL`.
        Normal,
        /// `MADV_RANDOM`. replaces libc::MADV_RANDOM.
        Random,
        /// `MADV_SEQUENTIAL`. replaces libc::MADV_SEQUENTIAL.
        Sequential,
        /// `MADV_WILLNEED`. replaces libc::MADV_WILLNEED.
        WillNeed,
        /// `MADV_DONTNEED`: on macOS the pages may be dropped, and keep counting in the
        /// footprint until they are. replaces libc::MADV_DONTNEED.
        DontNeed,
        /// `MADV_FREE`.
        Free,
        /// `MADV_FREE_REUSABLE`: the pages leave the footprint now; their bytes are undefined
        /// until [`Advice::FreeReuse`]. replaces libc::MADV_FREE_REUSABLE.
        FreeReusable,
        /// `MADV_FREE_REUSE`: pages given back by [`Advice::FreeReusable`] are used again and
        /// count in the footprint. replaces libc::MADV_FREE_REUSE.
        FreeReuse,
    }

    impl Advice {
        /// Its number (sys/mman.h).
        #[blaze::noalloc]
        #[blaze::nopanic]
        pub(in crate::posix::io) const fn raw(self) -> c_int {
            match self {
                Advice::Normal => 0,
                Advice::Random => 1,
                Advice::Sequential => 2,
                Advice::WillNeed => 3,
                Advice::DontNeed => 4,
                Advice::Free => 5,
                Advice::FreeReusable => 7,
                Advice::FreeReuse => 8,
            }
        }
    }

    /// `MAP_FAILED` (sys/mman.h): `(void *)-1`.
    const MAP_FAILED: usize = usize::MAX;

    /// The address libSystem mapped, as a pointer; `None` for address 0.
    #[blaze::noalloc]
    #[blaze::nopanic(trusted = "NonNull::new aborts only in new_unchecked's check that the pointer is not null, which new itself tests before it calls it")]
    fn mapped(at: *mut c_void) -> Option<NonNull<u8>> {
        NonNull::new(at.cast::<u8>())
    }

    /// `mmap`: `len` bytes mapped, near `hint` when given (exactly there with
    /// [`MapFlags::FIXED`]), of `file` from `offset` or of zero pages. The signature of
    /// [`crate::linux::mmap`]. replaces libc::mmap, libc::MAP_FAILED (the `Err`).
    ///
    /// # Safety
    ///
    /// With [`MapFlags::FIXED`] the range `hint..hint + len` holds nothing the program still
    /// uses. A shared mapping of a file is memory other processes can change under any
    /// reference made to it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn mmap(
        hint: Option<NonNull<u8>>,
        len: usize,
        prot: Prot,
        flags: MapFlags,
        file: Option<Fd>,
        offset: u64,
    ) -> Result<NonNull<u8>, Errno> {
        let hint = hint.map_or(core::ptr::null_mut(), |it| it.as_ptr().cast::<c_void>());
        let fd = file.map_or(-1, Fd::raw);
        let Ok(offset) = off_t::try_from(offset) else {
            return Err(Errno::Inval);
        };
        // SAFETY: the call changes only the address space; the caller answers for what a fixed
        // mapping replaces. The flag sets' bits are sys/mman.h's and fit an `int`.
        let at = unsafe { sys_mem::mmap(hint, len, prot.bits() as c_int, flags.bits() as c_int, fd, offset) };
        if at.addr() == MAP_FAILED {
            return Err(crate::posix::signal::errno());
        }
        // A successful mapping is never at address 0 (the zero page is reserved).
        mapped(at).ok_or(Errno::NoMem)
    }

    /// `munmap`: the pages of `at..at + len` unmapped. replaces libc::munmap.
    ///
    /// # Safety
    ///
    /// Nothing uses the range after the call: no reference, no stack, no code.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn munmap(at: NonNull<u8>, len: usize) -> Result<(), Errno> {
        // SAFETY: the call changes only the address space; the caller answers for the range.
        super::super::said(unsafe { sys_mem::munmap(at.as_ptr().cast(), len) }).map(|_| ())
    }

    /// `madvise`: `advice` about `at..at + len`, through the cold table. replaces
    /// libc::madvise.
    ///
    /// # Safety
    ///
    /// For [`Advice::DontNeed`], [`Advice::Free`] and [`Advice::FreeReusable`], nothing still
    /// needs the bytes of the range.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn madvise(at: NonNull<u8>, len: usize, advice: Advice) -> Result<(), Errno> {
        // SAFETY: the call changes only the pages of the range; the caller answers for them.
        super::super::said(unsafe { crate::macos::cold::madvise(at.as_ptr().cast(), len, advice.raw()) }).map(|_| ())
    }

    /// `mprotect`: the pages of `at..at + len` made usable for `prot` only. replaces
    /// libc::mprotect.
    ///
    /// # Safety
    ///
    /// No reference to the range allows a use `prot` takes away.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn mprotect(at: NonNull<u8>, len: usize, prot: Prot) -> Result<(), Errno> {
        // SAFETY: the call changes only the pages' protection; the caller answers for the range.
        super::super::said(unsafe { sys_mem::mprotect(at.as_ptr().cast(), len, prot.bits() as c_int) }).map(|_| ())
    }

    flag_set! {
        /// `msync`'s flags (sys/mman.h, macOS's numbers).
        MsyncFlags: u32 {
            /// `MS_ASYNC`: the write-back is begun, not waited for. replaces libc::MS_ASYNC.
            ASYNC = 0x0001;
            /// `MS_INVALIDATE`.
            INVALIDATE = 0x0002;
            /// `MS_SYNC`: the write-back is waited for.
            SYNC = 0x0010;
        }
    }

    /// `msync`'s call, macOS: libSystem's.
    ///
    /// # Safety
    ///
    /// As [`super::msync`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) unsafe fn msync_raw(at: *mut c_void, len: usize, flags: c_int) -> c_int {
        // SAFETY: the caller's contract.
        unsafe { sys_mem::msync(at, len, flags) }
    }

    /// `mincore`'s call, macOS: through the cold table (`char *vec`).
    ///
    /// # Safety
    ///
    /// As [`super::mincore`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) unsafe fn mincore_raw(at: *mut c_void, len: usize, held: *mut u8) -> c_int {
        // SAFETY: the caller's contract.
        unsafe { crate::macos::cold::mincore(at.cast_const(), len, held.cast()) }
    }
}

/// replaces libc::msync, libc::MS_ASYNC, libc::MS_INVALIDATE, libc::MS_SYNC, libc::mincore.
/// Linux: [`crate::linux::fdctl::msync`], [`crate::linux::fdctl::mincore`], raw system calls.
#[cfg(target_os = "linux")]
pub use crate::linux::fdctl::{MsyncFlags, mincore, msync};

/// `msync`: the dirty pages of the shared file mapping `at..at + len` written back to the
/// file, waited for or begun as `flags` says. replaces libc::msync.
///
/// # Safety
///
/// `at..at + len` is a mapping of this process, `at` page-aligned.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn msync(at: NonNull<u8>, len: usize, flags: MsyncFlags) -> Result<(), Errno> {
    // SAFETY: the kernel reads the page tables of the range and writes back its pages; the
    // caller answers for the range.
    let result = unsafe { on_macos::msync_raw(at.as_ptr().cast::<c_void>(), len, flags.bits() as c_int) };
    super::said(result).map(|_| ())
}

/// `mincore`: which pages of `at..at + len` are resident, one byte a page into `held` (bit 0
/// set for a resident page). [`Errno::NoMem`] where part of the range is not mapped. replaces
/// libc::mincore.
///
/// # Safety
///
/// `held` has a byte for every page of the range: at least `len` divided by the page size,
/// rounded up. The kernel writes that many bytes and does not check the slice's length.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn mincore(at: NonNull<u8>, len: usize, held: &mut [u8]) -> Result<(), Errno> {
    // SAFETY: the kernel reads the page tables of the range and writes one byte a page into
    // `held`, which the caller sized; it keeps no pointer.
    let result = unsafe { on_macos::mincore_raw(at.as_ptr().cast::<c_void>(), len, held.as_mut_ptr()) };
    super::said(result).map(|_| ())
}
