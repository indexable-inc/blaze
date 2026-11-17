//! Address space: what the page pool, the frames and a thread's stack are made of.

use core::ptr::NonNull;

use super::{fs::arg, nr, raw};
use crate::{Errno, Fd};

flag_set! {
    /// What a mapping's pages may be used for (`PROT_*`); [`Prot::NONE`] reserves.
    Prot: u32 {
        /// `PROT_READ`.
        READ = 1;
        /// `PROT_WRITE`.
        WRITE = 2;
        /// `PROT_EXEC`.
        EXEC = 4;
    }
}

flag_set! {
    /// `mmap`'s flags.
    MapFlags: u32 {
        /// `MAP_SHARED`.
        SHARED = 0x01;
        /// `MAP_PRIVATE`.
        PRIVATE = 0x02;
        /// `MAP_FIXED`: at the address given, replacing what is mapped there.
        FIXED = 0x10;
        /// `MAP_ANONYMOUS`: zero pages, of no file.
        ANONYMOUS = 0x20;
        /// `MAP_NORESERVE`: no swap is reserved; a reservation of address space.
        NO_RESERVE = 0x4000;
        /// `MAP_POPULATE`.
        POPULATE = 0x8000;
        /// `MAP_STACK`.
        STACK = 0x20000;
        /// `MAP_FIXED_NOREPLACE`: at the address given, or [`Errno::Exist`].
        FIXED_NO_REPLACE = 0x100000;
    }
}

/// What `madvise` tells the kernel about a range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Advice {
    /// `MADV_NORMAL`.
    Normal,
    /// `MADV_RANDOM`.
    Random,
    /// `MADV_SEQUENTIAL`.
    Sequential,
    /// `MADV_WILLNEED`.
    WillNeed,
    /// `MADV_DONTNEED`: the pages are given back now; an anonymous page reads as zero after.
    DontNeed,
    /// `MADV_FREE`: the pages may be taken back under pressure.
    Free,
    /// `MADV_HUGEPAGE`.
    HugePage,
    /// `MADV_NOHUGEPAGE`.
    NoHugePage,
    /// `MADV_DONTDUMP`.
    DontDump,
}

impl Advice {
    const fn raw(self) -> usize {
        match self {
            Advice::Normal => 0,
            Advice::Random => 1,
            Advice::Sequential => 2,
            Advice::WillNeed => 3,
            Advice::DontNeed => 4,
            Advice::Free => 8,
            Advice::HugePage => 14,
            Advice::NoHugePage => 15,
            Advice::DontDump => 16,
        }
    }
}

/// `mmap`: `len` bytes mapped, near `hint` when given (exactly there with
/// [`MapFlags::FIXED`]), of `file` from `offset` or of zero pages. Used by the page pool's
/// reservation, a frame's slab, a thread's stack and the io_uring rings.
///
/// # Safety
///
/// With [`MapFlags::FIXED`] the range `hint..hint + len` holds nothing the program still
/// uses. A shared mapping of a file is memory other processes can change under any reference
/// made to it.
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
    let hint = hint.map_or(0, |it| it.as_ptr() as usize);
    let fd = file.map_or(usize::MAX, arg);
    // SAFETY: the kernel changes only the address space; the caller answers for what a fixed
    // mapping replaces.
    let at = raw::check(unsafe {
        raw::syscall6(nr::MMAP, hint, len, prot.bits() as usize, flags.bits() as usize, fd, offset as usize)
    })?;
    // A successful mapping is never at address 0 (`mmap_min_addr`).
    mapped(at).ok_or(Errno::NoMem)
}

/// The address the kernel mapped, as a pointer; `None` for address 0.
#[blaze::noalloc]
#[blaze::nopanic(trusted = "NonNull::new aborts only in new_unchecked's check that the pointer is not null, which new itself tests before it calls it")]
fn mapped(at: usize) -> Option<NonNull<u8>> {
    NonNull::new(at as *mut u8)
}

/// `munmap`: the pages of `at..at + len` unmapped.
///
/// # Safety
///
/// Nothing uses the range after the call: no reference, no stack, no code.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn munmap(at: NonNull<u8>, len: usize) -> Result<(), Errno> {
    // SAFETY: the kernel changes only the address space; the caller answers for the range.
    raw::check(unsafe { raw::syscall2(nr::MUNMAP, at.as_ptr() as usize, len) }).map(|_| ())
}

/// `madvise`: `advice` about `at..at + len`. Used by the page pool's eviction
/// ([`Advice::DontNeed`]) and its read-ahead.
///
/// # Safety
///
/// For [`Advice::DontNeed`] and [`Advice::Free`], nothing still needs the bytes of the range.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn madvise(at: NonNull<u8>, len: usize, advice: Advice) -> Result<(), Errno> {
    // SAFETY: the kernel changes only the pages of the range; the caller answers for them.
    raw::check(unsafe { raw::syscall3(nr::MADVISE, at.as_ptr() as usize, len, advice.raw()) })
        .map(|_| ())
}

/// `mprotect`: the pages of `at..at + len` made usable for `prot` only. Used by a stack's
/// guard page and by executable memory (written, then made executable, never both).
///
/// # Safety
///
/// No reference to the range allows a use `prot` takes away.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn mprotect(at: NonNull<u8>, len: usize, prot: Prot) -> Result<(), Errno> {
    // SAFETY: the kernel changes only the pages' protection; the caller answers for the range.
    raw::check(unsafe { raw::syscall3(nr::MPROTECT, at.as_ptr() as usize, len, prot.bits() as usize) })
        .map(|_| ())
}
