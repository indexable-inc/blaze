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
        /// `MAP_HUGETLB`: backed by huge pages from the hugetlbfs pool. replaces libc::MAP_HUGETLB
        HUGETLB = 0x40000;
        /// `MAP_HUGE_2MB` (`21 << MAP_HUGE_SHIFT`): with [`MapFlags::HUGETLB`], 2 MiB pages.
        /// replaces libc::MAP_HUGE_2MB
        HUGE_2MB = 21 << 26;
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
    /// `MADV_DONTFORK`: a child made by `fork` does not get the range. replaces
    /// libc::MADV_DONTFORK
    DontFork,
    /// `MADV_POPULATE_WRITE` (5.14): the range's pages faulted in writable now, as writes
    /// would, so later writes take no fault. [`Errno::Inval`] before 5.14.
    /// replaces libc::MADV_POPULATE_WRITE
    PopulateWrite,
    /// `MADV_COLLAPSE` (6.1): the range's pages collapsed into transparent huge pages now,
    /// synchronously, whatever the THP setting says.
    /// replaces libc::MADV_COLLAPSE
    Collapse,
}

impl Advice {
    const fn raw(self) -> usize {
        match self {
            Advice::PopulateWrite => 23,
            Advice::Collapse => 25,
            Advice::Normal => 0,
            Advice::Random => 1,
            Advice::Sequential => 2,
            Advice::WillNeed => 3,
            Advice::DontNeed => 4,
            Advice::Free => 8,
            Advice::HugePage => 14,
            Advice::NoHugePage => 15,
            Advice::DontDump => 16,
            Advice::DontFork => 10,
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
/// uses, or, where it is used (code a thread may be running), the new mapping's bytes are
/// equal to the old ones at every address with the same or wider protection: the kernel
/// replaces the range's pages under its mmap lock in one call, so a reader sees the old bytes
/// or the new, which are the same. A shared mapping of a file is memory other processes can change under any reference
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

/// `PIDFD_SELF_THREAD_GROUP` (linux/pidfd.h, 6.14): the calling process as a pidfd, with no
/// descriptor opened, for the calls that take a pidfd ([`process_madvise`]).
/// replaces blaze_mem::os's private PIDFD_SELF_THREAD_GROUP (-10001; the libc crate has none)
pub const PIDFD_SELF_THREAD_GROUP: Fd = Fd::new(-10001);

/// `process_madvise(pidfd, ranges, advice, 0)`: `advice` about each of `ranges` (an address
/// and a length each, the kernel's `iovec`) of the process `pidfd` names, in one call: how
/// many bytes were advised, which may be fewer than all (the caller continues from there).
/// [`PIDFD_SELF_THREAD_GROUP`] names this process. Used by blaze-mem's batched eviction
/// ([`Advice::DontNeed`]).
///
/// # Safety
///
/// For this process and [`Advice::DontNeed`] or [`Advice::Free`]: nothing still needs the
/// bytes of any range; each range is page-aligned.
/// replaces libc::syscall(libc::SYS_process_madvise) (as blaze_sys::linux::process_madvise(PIDFD_SELF_THREAD_GROUP, &[[addr, len]], Advice::DontNeed); flags 0; returns Result<usize bytes, Errno>)
/// replaces libc::SYS_process_madvise (the call; the number as data stays nr::PROCESS_MADVISE)
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn process_madvise(pidfd: Fd, ranges: &[[usize; 2]], advice: Advice) -> Result<usize, Errno> {
    // SAFETY: `ranges` is that many live `iovec`s (two `usize`s each, the kernel's layout)
    // the kernel only reads; the caller answers for the ranges' bytes.
    raw::check(unsafe {
        raw::syscall5(nr::PROCESS_MADVISE, arg(pidfd), ranges.as_ptr() as usize, ranges.len(), advice.raw(), 0)
    })
}

/// A memory policy's mode (`MPOL_*`, linux/mempolicy.h), for [`mbind`].
/// replaces libc::MPOL_PREFERRED, libc::MPOL_BIND, libc::MPOL_DEFAULT, libc::MPOL_INTERLEAVE, libc::MPOL_LOCAL
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemPolicy {
    /// `MPOL_DEFAULT` (0): the process's policy.
    Default,
    /// `MPOL_PREFERRED` (1): the first node of the mask first, others when it is full.
    Preferred,
    /// `MPOL_BIND` (2): only the mask's nodes.
    Bind,
    /// `MPOL_INTERLEAVE` (3): page by page across the mask's nodes.
    Interleave,
    /// `MPOL_LOCAL` (4): the node of the CPU that faults the page.
    Local,
}

impl MemPolicy {
    /// The kernel's number.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn raw(self) -> usize {
        match self {
            MemPolicy::Default => 0,
            MemPolicy::Preferred => 1,
            MemPolicy::Bind => 2,
            MemPolicy::Interleave => 3,
            MemPolicy::Local => 4,
        }
    }
}

/// `mbind(at, len, policy, nodes, bits + 1, 0)`: the NUMA policy of `at..at + len` set to
/// `policy` over the nodes whose bits are set in `nodes` (bit `n` of word `n / 64`). The
/// kernel reads one bit fewer than its `maxnode` says; this passes `64 * nodes.len() + 1`, so
/// every bit of `nodes` is read. Flags 0: pages already placed stay where they are.
/// [`Errno::Inval`] for a node the machine lacks.
///
/// # Safety
///
/// `at..at + len` is a page-aligned range of this process that the caller owns; a policy
/// moves no byte, but it is the caller's to set.
/// replaces libc::syscall(libc::SYS_mbind) (as blaze_sys::linux::mbind(at, len, MemPolicy::Preferred, &mask); flags 0)
/// replaces libc::SYS_mbind (the call; the number as data stays nr::MBIND)
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn mbind(at: NonNull<u8>, len: usize, policy: MemPolicy, nodes: &[u64]) -> Result<(), Errno> {
    let maxnode = nodes.len().saturating_mul(64).saturating_add(1);
    // SAFETY: `nodes` is `nodes.len()` live words the kernel reads (`maxnode - 1` bits); the
    // caller answers for the range, whose bytes the call does not touch.
    raw::check(unsafe {
        raw::syscall6(nr::MBIND, at.as_ptr() as usize, len, policy.raw(), nodes.as_ptr() as usize, maxnode, 0)
    })
    .map(|_| ())
}

#[cfg(test)]
mod advice_tests {
    use super::{
        Advice, MapFlags, MemPolicy, PIDFD_SELF_THREAD_GROUP, Prot, madvise, mbind, mmap, munmap, process_madvise,
    };
    use crate::Errno;

    /// The new advice and policy numbers are libc's; a populate, a batched give-back of
    /// this process's pages and a policy on node 0 each work where the kernel has them.
    #[test]
    fn advice_policies_and_batched_advice() {
        assert_eq!(Advice::PopulateWrite.raw(), libc::MADV_POPULATE_WRITE as usize);
        assert_eq!(Advice::Collapse.raw(), libc::MADV_COLLAPSE as usize);
        for (ours, theirs) in [
            (MemPolicy::Default, libc::MPOL_DEFAULT),
            (MemPolicy::Preferred, libc::MPOL_PREFERRED),
            (MemPolicy::Bind, libc::MPOL_BIND),
            (MemPolicy::Interleave, libc::MPOL_INTERLEAVE),
            (MemPolicy::Local, libc::MPOL_LOCAL),
        ] {
            assert_eq!(ours.raw(), theirs as usize);
        }
        const LEN: usize = 1 << 16; // whole pages on 4, 16 and 64 KiB kernels
        // SAFETY: a new anonymous mapping the kernel places; nothing else uses it.
        let at = unsafe {
            mmap(None, LEN, Prot::READ | Prot::WRITE, MapFlags::PRIVATE | MapFlags::ANONYMOUS, None, 0)
        }
        .unwrap();
        // SAFETY: the mapping above, whose bytes no one needs.
        match unsafe { madvise(at, LEN, Advice::PopulateWrite) } {
            Ok(()) | Err(Errno::Inval) => {}
            Err(other) => panic!("MADV_POPULATE_WRITE: {other}"),
        }
        // SAFETY: as above.
        match unsafe { process_madvise(PIDFD_SELF_THREAD_GROUP, &[[at.as_ptr() as usize, LEN]], Advice::DontNeed) } {
            Ok(given) => assert_eq!(given, LEN),
            // Before 6.14 (no PIDFD_SELF), before 5.10 (no call), or under a filter.
            Err(Errno::BadF | Errno::NoSys | Errno::Perm | Errno::Inval) => {}
            Err(other) => panic!("process_madvise: {other}"),
        }
        // SAFETY: as above.
        match unsafe { mbind(at, LEN, MemPolicy::Preferred, &[1]) } {
            // NoSys: a kernel built without NUMA.
            Ok(()) | Err(Errno::NoSys | Errno::Perm) => {}
            Err(other) => panic!("mbind: {other}"),
        }
        // SAFETY: the mapping above; nothing uses it now.
        unsafe { munmap(at, LEN) }.unwrap();
    }
}

/// One `membarrier` command (`MEMBARRIER_CMD_*`, linux/membarrier.h).
/// replaces blaze_launch_rt::sys::linux's private PRIVATE_EXPEDITED_SYNC_CORE and REGISTER_PRIVATE_EXPEDITED_SYNC_CORE (the libc crate has the MEMBARRIER_CMD_* numbers on Android only)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Membarrier {
    /// `MEMBARRIER_CMD_QUERY` (0): the commands this kernel has, as a bit set.
    Query,
    /// `MEMBARRIER_CMD_PRIVATE_EXPEDITED` (1 << 3): every running thread of this process has
    /// passed a full memory barrier when the call returns. Needs the registration.
    PrivateExpedited,
    /// `MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED` (1 << 4).
    RegisterPrivateExpedited,
    /// `MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE` (1 << 5): as `PrivateExpedited`, and each
    /// thread has run a core-serializing instruction, so none runs instructions fetched before
    /// a write of code (4.16 on aarch64). Needs the registration.
    PrivateExpeditedSyncCore,
    /// `MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE` (1 << 6).
    RegisterPrivateExpeditedSyncCore,
}

impl Membarrier {
    /// The kernel's number.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn raw(self) -> u32 {
        match self {
            Membarrier::Query => 0,
            Membarrier::PrivateExpedited => 1 << 3,
            Membarrier::RegisterPrivateExpedited => 1 << 4,
            Membarrier::PrivateExpeditedSyncCore => 1 << 5,
            Membarrier::RegisterPrivateExpeditedSyncCore => 1 << 6,
        }
    }
}

/// `membarrier(command, 0, 0)`: the command's answer (the set of commands for
/// [`Membarrier::Query`], 0 otherwise). [`Errno::Perm`] for an expedited command before its
/// registration; [`Errno::Inval`] for a command this kernel lacks; [`Errno::NoSys`] before
/// 4.3 or under a filter. Used by blaze-launch-rt's code patcher on aarch64.
/// replaces libc::syscall(libc::SYS_membarrier) (flags 0, cpu 0; as blaze_sys::linux::membarrier(Membarrier::..); returns Result<u32, Errno>, the site's `== 0` check becomes the Ok)
/// replaces libc::SYS_membarrier (the call; the number as data stays nr::MEMBARRIER)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn membarrier(command: Membarrier) -> Result<u32, Errno> {
    // SAFETY: the call takes three numbers and touches no memory of this process.
    raw::check(unsafe { raw::syscall3(nr::MEMBARRIER, command.raw() as usize, 0, 0) }).map(|answer| answer as u32)
}

#[cfg(test)]
mod membarrier_tests {
    use super::{Membarrier, membarrier};
    use crate::Errno;

    /// The query names the sync-core commands where the kernel has them, and the barrier is
    /// refused before its registration and made after it.
    #[test]
    fn a_sync_core_barrier_needs_its_registration() {
        let has = match membarrier(Membarrier::Query) {
            Ok(has) => has,
            Err(Errno::NoSys) => {
                eprintln!("skipped: this host refuses membarrier");
                return;
            }
            Err(other) => panic!("membarrier(QUERY): {other}"),
        };
        let both = Membarrier::PrivateExpeditedSyncCore.raw() | Membarrier::RegisterPrivateExpeditedSyncCore.raw();
        if has & both != both {
            eprintln!("skipped: this kernel has no sync-core membarrier ({has:#x})");
            return;
        }
        assert_eq!(membarrier(Membarrier::RegisterPrivateExpeditedSyncCore), Ok(0));
        assert_eq!(membarrier(Membarrier::PrivateExpeditedSyncCore), Ok(0));
    }
}
