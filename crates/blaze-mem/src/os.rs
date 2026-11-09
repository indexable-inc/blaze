//! The kernel's virtual memory: reserve address space, commit pages of it, give pages back,
//! release it. Linux and macOS through `mmap`/`mprotect`/`madvise`, Windows through
//! `VirtualAlloc`/`VirtualFree`. Every other function of the crate reaches the kernel through
//! these. On wasm there is no address space to reserve. On wasm64 (the bootstrap seed) a
//! reservation is linear memory grown at its end (`memory.grow`), which the seed's runner
//! (`blaze-seed-run`) commits only where it is touched, and pages go back through the runner's
//! `blaze_seed::decommit`. Elsewhere (wasm32, whose hosts have no such import) a reservation is
//! memory taken whole from the global allocator.
//!
//! A reservation costs address space and a mapping, no memory. On macOS it is `PROT_NONE`
//! (Windows `PAGE_NOACCESS`, `MEM_RESERVE`), and committing makes a prefix readable and
//! writable and charges it to the commit limit. On Linux it is readable and writable from the
//! start (`MAP_NORESERVE`, so nothing is charged before a page is touched), and committing is
//! a prefault of the pages about to be used (`MADV_POPULATE_WRITE`), never an `mprotect`:
//! every `mprotect`, `munmap` and `MAP_FIXED` takes the process's `mmap_lock` for writing,
//! and on 2026-11-05 a cold `blaze check` on 64 cores spent 99.7% of its CPU in the fault
//! handler behind those writers (each `madvise` and `mmap` taking 4 to 5 s), every worker's
//! first touch of the node column falling back to the lock its growth's `mprotect` held. A
//! populate and a decommit (`MADV_DONTNEED`) take the lock for reading only. Every Unix
//! reservation is aligned to [`HUGE_PAGE`](crate::HUGE_PAGE). On Linux one made of
//! transparent huge pages is advised `MADV_HUGEPAGE`, so the kernel backs each 2 MB of it
//! committed whole with one huge page; one made of the reserved pool commits its huge pages
//! with `MAP_HUGETLB` ([`commit_reserved`]). macOS has no huge pages for anonymous memory on
//! arm64, and Windows' large pages must be committed whole at reservation (`MEM_LARGE_PAGES`),
//! which commit-on-growth cannot do, so there a reservation is made of base pages
//! ([`crate::huge`]).
//!
//! A reservation can prefer a NUMA node for its pages ([`reserve_on`], [`bind`]): Linux's
//! `mbind`, Windows' `VirtualAllocExNuma` at reservation; macOS has one memory node.

use std::{io, ptr::NonNull, sync::atomic};

#[cfg(unix)]
use crate::syscalls::{Kind, timed};

/// Alignment of every reservation: a huge page on Unix (so a huge region starts on one);
/// Windows' allocation granularity, which `VirtualAlloc` always gives.
#[cfg(unix)]
pub(crate) const RESERVE_ALIGN: usize = crate::HUGE_PAGE;
#[cfg(not(unix))]
pub(crate) const RESERVE_ALIGN: usize = 64 << 10;

static PAGE: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

pub(crate) fn page_size() -> usize {
    let page = PAGE.load(atomic::Ordering::Relaxed);
    if page != 0 {
        return page;
    }
    let page = query_page_size();
    PAGE.store(page, atomic::Ordering::Relaxed);
    page
}

#[cfg(unix)]
fn query_page_size() -> usize {
    // SAFETY: `sysconf` reads a constant of the system; it has no memory preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).ok().filter(|page| page.is_power_of_two()).unwrap_or(4096)
}

/// Windows' page is 4 KB on every architecture it runs blaze on (x86-64, arm64); without
/// virtual memory (wasm32) it is only the unit regions commit in.
#[cfg(all(not(unix), not(target_arch = "wasm64")))]
fn query_page_size() -> usize {
    4096
}

/// On wasm64 a page is the wasm page, 64 KiB: what regions purge and decommit in, through the
/// seed runner's `decommit`. Every host page size (4 KiB, Apple silicon's 16 KiB) divides it,
/// so a purge never reaches the runner as part of a host page, which it would have to zero by
/// writing it, making it resident.
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
fn query_page_size() -> usize {
    64 << 10
}

/// Reserves `bytes` (a multiple of [`page_size`]) of address space, aligned to
/// [`RESERVE_ALIGN`], none of it committed. With `huge`, the kernel is asked to back it with
/// transparent huge pages where it can.
#[cfg(unix)]
pub(crate) fn reserve(bytes: usize, huge: bool) -> io::Result<NonNull<u8>> {
    #[cfg(target_os = "linux")]
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    #[cfg(not(target_os = "linux"))]
    let prot = libc::PROT_NONE;
    reserve_with(bytes, huge, prot)
}

/// Whether a reservation is readable and writable from the start, so a commit only counts and
/// prefaults ([`commit`]) and may come after the length that covers it is published: Linux.
pub(crate) const RESERVATION_WRITABLE: bool = cfg!(target_os = "linux");

/// Reserves `bytes` (a multiple of [`page_size`]) of address space, aligned to
/// [`RESERVE_ALIGN`], readable and writable from the start and reading zeros: a sparse table's
/// (a page directory's), of which only the entries written ever cost memory. Linux and macOS
/// back a page on first touch and charge nothing before (`MAP_NORESERVE`); Linux is told not
/// to make it of transparent huge pages, which would turn the first entry written into 2 MiB
/// resident. Windows can make a range accessible only by committing it, so there it is
/// committed whole and charged ([`crate::Region::reserve_sparse`] counts it so); without
/// virtual memory it is [`reserve`], which reads zeros already.
#[cfg(unix)]
pub(crate) fn reserve_sparse(bytes: usize) -> io::Result<NonNull<u8>> {
    // Base pages, advised so by `reserve_with` (`advise_pages`).
    reserve_with(bytes, false, libc::PROT_READ | libc::PROT_WRITE)
}

#[cfg(windows)]
pub(crate) fn reserve_sparse(bytes: usize) -> io::Result<NonNull<u8>> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    // SAFETY: a fresh reservation at an address the kernel picks, committed whole.
    let base =
        unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn reserve_sparse(bytes: usize) -> io::Result<NonNull<u8>> {
    reserve(bytes, false)
}

/// [`reserve`] with the pages' protection `prot`.
#[cfg(unix)]
fn reserve_with(bytes: usize, huge: bool, prot: libc::c_int) -> io::Result<NonNull<u8>> {
    let padded = bytes
        .checked_add(RESERVE_ALIGN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "reservation overflows"))?;
    // SAFETY: a fresh anonymous mapping at an address the kernel picks: it overlaps nothing the
    // program holds.
    let base = timed(Kind::Reserve, || unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            padded,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    });
    if base == libc::MAP_FAILED {
        return Err(refused_reservation(padded, prot, io::Error::last_os_error()));
    }
    let base = base.cast::<u8>();
    let head = base.align_offset(RESERVE_ALIGN);
    let tail = padded - head - bytes;
    let aligned = base.wrapping_add(head);
    if head != 0 {
        // SAFETY: `[base, aligned)` is the front of the mapping just made, which nothing else
        // knows of.
        let _ = timed(Kind::Munmap, || unsafe { libc::munmap(base.cast(), head) });
    }
    if tail != 0 {
        // SAFETY: `[aligned + bytes, base + padded)` is the back of the same fresh mapping.
        let _ = timed(Kind::Munmap, || unsafe { libc::munmap(aligned.wrapping_add(bytes).cast(), tail) });
    }
    #[cfg(target_os = "linux")]
    {
        advise_pages(aligned, bytes, huge);
        if huge && prot != libc::PROT_NONE && bytes > crate::HUGE_PAGE {
            // A writable reservation advised huge pages whole would make its first touch a
            // 2 MiB page, so a small huge region would cost a huge page: its first huge page's
            // worth stays base pages until it grows past it ([`promote_first`]).
            advise_pages(aligned, crate::HUGE_PAGE, false);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = huge;
    NonNull::new(aligned).ok_or_else(|| io::Error::other("mmap returned null"))
}

/// A refused reservation, named: on Linux a writable one is charged to the commit limit at once
/// when the system ignores `MAP_NORESERVE` (`vm.overcommit_memory = 2`), which a reservation of
/// tens of GiB passes.
#[cfg(unix)]
#[cold]
fn refused_reservation(bytes: usize, prot: libc::c_int, why: io::Error) -> io::Error {
    if !(cfg!(target_os = "linux")
        && prot != libc::PROT_NONE
        && why.raw_os_error() == Some(libc::ENOMEM))
    {
        return why;
    }
    let mode = std::fs::read_to_string("/proc/sys/vm/overcommit_memory")
        .map(|it| it.trim().to_owned()) // alloc: the error of a refused reservation, once
        .unwrap_or_else(|read| format!("unreadable: {read}")); // alloc: as above
    // alloc: the error of a refused reservation, once
    io::Error::new(
        why.kind(),
        format!(
            "reserving {bytes} bytes of address space readable and writable (mmap MAP_NORESERVE): {why}; \
             vm.overcommit_memory is {mode}, and blaze needs 0 or 1 (2 charges every reservation whole)"
        ),
    )
}

/// Tells Linux which pages `[ptr, ptr + bytes)` is made of: with `huge`, transparent huge
/// pages (`MADV_HUGEPAGE`); without, base pages only (`MADV_NOHUGEPAGE`), whatever the host's
/// `transparent_hugepage/enabled` says. A base-page region is touched sparsely (a table's
/// first entries, a stack's top, a bump arena's head), and under `always` the kernel would
/// back each first touch with 2 MiB, and khugepaged would collapse any 2 MiB with one page in
/// it (`max_ptes_none` is 511 by default): an ix VM's daemon reached 60 GiB that way, against
/// 25 GiB under `madvise`. Advice only: a kernel without transparent huge pages says EINVAL,
/// and has the base pages asked for.
///
/// A fresh mapping over part of a range (a hugetlb range's `MAP_FIXED`) loses the advice, so
/// each such mapping is advised again.
#[cfg(target_os = "linux")]
fn advise_pages(ptr: *mut u8, bytes: usize, huge: bool) {
    let advice = if huge { libc::MADV_HUGEPAGE } else { libc::MADV_NOHUGEPAGE };
    // SAFETY: the caller's range is a mapping of its own; advice changes no contents.
    let _ = timed(Kind::Advise, || unsafe { libc::madvise(ptr.cast(), bytes, advice) });
}

/// Keeps `[ptr, ptr + bytes)`, a mapping its caller made outside a region (a fiber's stack),
/// on base pages whatever the host's transparent huge page setting: see [`advise_pages`].
/// Nothing elsewhere.
///
/// # Safety
///
/// The range is a live mapping the caller owns.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn keep_base_pages(ptr: NonNull<u8>, bytes: usize) {
    advise_pages(ptr.as_ptr(), bytes, false);
}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn keep_base_pages(_ptr: NonNull<u8>, _bytes: usize) {}

/// What the process tells Linux about transparent huge pages in memory nothing advised (the
/// global allocator's when it is not on huge pages, thread stacks, libc's own mappings),
/// given the pages its huge regions are made of ([`crate::huge::Thp`]).
///
/// - `Pages::Transparent`: only advised memory gets them, as under the
///   host setting `madvise` (`PR_SET_THP_DISABLE` with `PR_THP_DISABLE_EXCEPT_ADVISED`, Linux
///   6.18). An older kernel refuses the flag; then every region of ours is still advised one
///   by one ([`advise_pages`]) and the rest follows the host, which [`Thp::Unavailable`]
///   says in the memory profile.
/// - `Pages::Base` and `Pages::Reserved` (hugetlb, which this does not touch): none at all
///   (`PR_SET_THP_DISABLE`, Linux 3.15).
///
/// Linux keeps the setting across `fork` and `execve`, so a child process (rustc, the linker)
/// runs under it too.
///
/// [`Thp::Unavailable`]: crate::huge::Thp::Unavailable
#[cfg(target_os = "linux")]
pub(crate) fn opt_out_unadvised(pages: crate::huge::Pages) -> crate::huge::Thp {
    use crate::huge::{Pages, Thp};
    /// `prctl.h`'s `PR_THP_DISABLE_EXCEPT_ADVISED` (Linux 6.18).
    const PR_THP_DISABLE_EXCEPT_ADVISED: libc::c_ulong = 1 << 1;
    let (flags, then) = match pages {
        Pages::Transparent => (PR_THP_DISABLE_EXCEPT_ADVISED, Thp::AdvisedOnly),
        Pages::Base | Pages::Reserved => (0, Thp::Off),
    };
    let (on, unused): (libc::c_ulong, libc::c_ulong) = (1, 0);
    // SAFETY: sets one flag of this process's memory descriptor; reads and writes no memory.
    let set = unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, on, flags, unused, unused) };
    if set == 0 { then } else { Thp::Unavailable }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn opt_out_unadvised(_pages: crate::huge::Pages) -> crate::huge::Thp {
    crate::huge::Thp::NotLinux
}

#[cfg(windows)]
pub(crate) fn reserve(bytes: usize, _huge: bool) -> io::Result<NonNull<u8>> {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    // SAFETY: a fresh reservation at an address the kernel picks.
    let base = unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE, PAGE_NOACCESS) };
    NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)
}

/// On wasm64 (linear memory only grows at its end) a reservation is whole 64 KiB pages
/// ([`RESERVE_ALIGN`]) of linear memory reading zeros: a released range ([`freed`]) when one
/// is big enough, else fresh pages grown at the end with `memory.grow`. Either way it never
/// moves, as on the kernels, and nothing writes it: under the seed's runner a page costs
/// memory only once it is touched (`blaze-seed-run`'s `memory.rs`), and a released range was
/// decommitted, so a reservation costs address space alone. Never the global allocator, whose
/// reuse would zero a range by writing it and make it all resident.
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
pub(crate) fn reserve(bytes: usize, _huge: bool) -> io::Result<NonNull<u8>> {
    use core::arch::wasm64::memory_grow;
    let pages = bytes.div_ceil(RESERVE_ALIGN).max(1);
    if let Some(at) = freed::take(pages) {
        // As below: an address in the module's own memory, from the free list's answer.
        let base = std::ptr::with_exposed_provenance_mut::<u8>(at * RESERVE_ALIGN);
        return NonNull::new(base)
            .ok_or_else(|| io::Error::other("the free list gave pages at address 0"));
    }
    let old = memory_grow::<0>(pages);
    if old == usize::MAX {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            // alloc: the error of a reservation refused, once
            format!(
                "growing linear memory by {pages} pages of 64 KiB for a {bytes}-byte reservation (memory.grow) failed"
            ),
        ));
    }
    // The pages are the module's own memory at `old` pages in: an address, with nothing to
    // derive it from but the instruction's answer.
    let base = std::ptr::with_exposed_provenance_mut::<u8>(old * RESERVE_ALIGN);
    NonNull::new(base).ok_or_else(|| io::Error::other("memory.grow gave pages at address 0"))
}

/// Without virtual memory or the seed's runner (wasm32's linear memory only grows at its end) a
/// reservation is the whole range taken at once, zeroed, from the global allocator: it never
/// moves, as on the kernels, but costs its full size from the start.
#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
pub(crate) fn reserve(bytes: usize, _huge: bool) -> io::Result<NonNull<u8>> {
    let layout = whole(bytes)?;
    // SAFETY: `layout` has a nonzero size (`whole` refuses zero).
    let base = unsafe { std::alloc::alloc_zeroed(layout) };
    NonNull::new(base).ok_or_else(|| {
        io::Error::new(io::ErrorKind::OutOfMemory, "the reservation is more than memory holds")
    })
}

/// Released ranges of linear memory, in 64 KiB pages, by address, neighbours merged: linear
/// memory never shrinks, so [`release`] decommits a range and files it here for the next
/// [`reserve`]. Without it a seed that makes and drops reservations (link's 7,028 regions of
/// about 126 MiB each at native blaze's peak, docs/future/bootstrap.md) would grow its memory by
/// each one until it hit the runner's maximum.
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
mod freed {
    use std::collections::BTreeMap;

    /// First page to page count. A `BTreeMap` because merging a range with its neighbours
    /// needs them by address.
    #[expect(
        clippy::disallowed_types,
        reason = "below blaze-core: blaze-mem is a dependency of blaze-core; the released ranges, taken only as a reservation is made or released"
    )]
    static FREE: parking_lot::Mutex<BTreeMap<usize, usize>> =
        parking_lot::Mutex::new(BTreeMap::new());

    /// The first page of the smallest released range of at least `pages`, its tail filed
    /// again, or `None` when no range is big enough.
    pub(super) fn take(pages: usize) -> Option<usize> {
        let mut free = FREE.lock();
        let (&at, &have) = free
            .iter()
            .filter(|&(_, &have)| have >= pages)
            .min_by_key(|&(&at, &have)| (have, at))?;
        free.remove(&at);
        if have > pages {
            free.insert(at + pages, have - pages); // alloc: the free list's node, global as the list is
        }
        Some(at)
    }

    /// Files `[at, at + pages)`, merged with a released neighbour on either side. A range that
    /// overlaps one already filed was released twice, or is not what `reserve` gave: filing it
    /// would hand the same pages to two regions, so the process aborts, naming both.
    pub(super) fn give(mut at: usize, mut pages: usize) {
        let mut free = FREE.lock();
        let before = free.range(..at).next_back().filter(|&(&before, &len)| before + len > at);
        let after = free.range(at..).next().filter(|&(&next, _)| next < at + pages);
        if let Some((&filed, &len)) = before.or(after) {
            let page = super::RESERVE_ALIGN;
            eprintln!(
                "blaze-mem: the reservation released at {:#x}..{:#x} overlaps the released range {:#x}..{:#x} \
                 already on the free list: released twice, or not as reserved; aborting rather than give its pages \
                 to two regions",
                at * page,
                (at + pages) * page,
                filed * page,
                (filed + len) * page
            );
            std::process::abort();
        }
        if let Some((&before, &len)) = free.range(..at).next_back()
            && before + len == at
        {
            free.remove(&before);
            at = before;
            pages += len;
        }
        if let Some(len) = free.remove(&(at + pages)) {
            pages += len;
        }
        free.insert(at, pages); // alloc: the free list's node, global as the list is
    }
}

/// The seed runner's import that gives pages back (`blaze-seed-run`'s `RUNNER`).
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
mod runner {
    #[link(wasm_import_module = "blaze_seed")]
    unsafe extern "C" {
        /// Gives the pages of `[ptr, ptr + bytes)` back to the host; the range reads as zeros
        /// after. A range outside linear memory traps, naming it.
        pub(super) fn decommit(ptr: *mut u8, bytes: usize);
    }
}

/// As [`reserve`], with the pages the range is backed by preferred on NUMA node `node`.
///
/// Linux binds the fresh reservation (`mbind`, `MPOL_PREFERRED`) before any page of it exists,
/// so every page is made on `node` while it has room. Windows takes the node when the range is
/// reserved (`VirtualAllocExNuma`) and makes its pages there as they are first touched. macOS
/// has no placement call and one memory node on every Mac blaze runs on (Apple silicon's unified
/// memory, single-socket Intel), so node 0 is a plain reservation and any other is an error;
/// likewise without virtual memory.
#[cfg(target_os = "linux")]
pub(crate) fn reserve_on(bytes: usize, huge: bool, node: u32) -> io::Result<NonNull<u8>> {
    let base = reserve(bytes, huge)?;
    // SAFETY: `[base, base + bytes)` is the reservation just made, which nothing else knows of.
    if let Err(why) = unsafe { bind(base, bytes, node) } {
        // SAFETY: the same reservation, released whole, unused.
        unsafe { release(base, bytes) };
        return Err(why);
    }
    Ok(base)
}

#[cfg(windows)]
pub(crate) fn reserve_on(bytes: usize, _huge: bool, node: u32) -> io::Result<NonNull<u8>> {
    use windows_sys::Win32::System::{
        Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAllocExNuma},
        Threading::GetCurrentProcess,
    };
    // SAFETY: a fresh reservation in this process at an address the kernel picks; the node is a
    // number the kernel checks.
    let base = unsafe {
        VirtualAllocExNuma(
            GetCurrentProcess(),
            std::ptr::null(),
            bytes,
            MEM_RESERVE,
            PAGE_NOACCESS,
            node,
        )
    };
    NonNull::new(base.cast::<u8>()).ok_or_else(|| {
        let why = io::Error::last_os_error();
        // alloc: the error of a reservation refused, once
        io::Error::new(
            why.kind(),
            format!("reserving {bytes} bytes on NUMA node {node} (VirtualAllocExNuma): {why}"),
        )
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn reserve_on(bytes: usize, huge: bool, node: u32) -> io::Result<NonNull<u8>> {
    one_node(node)?;
    reserve(bytes, huge)
}

/// Prefers NUMA node `node` for the pages of `[ptr, ptr + bytes)` not yet made: Linux's `mbind`
/// with `MPOL_PREFERRED`. A kernel built without NUMA (`ENOSYS`) has node 0 alone, where its
/// memory already is; any other node is an error there.
///
/// # Safety
///
/// The range is inside one live reservation of [`reserve`]; changing its policy touches no data.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn bind(ptr: NonNull<u8>, bytes: usize, node: u32) -> io::Result<()> {
    /// `mempolicy.h`'s `MPOL_PREFERRED`.
    const MPOL_PREFERRED: libc::c_long = 1;
    /// The nodes a mask names: 1024.
    const MASK_WORDS: usize = 16;
    let bits = libc::c_ulong::BITS as usize;
    let at = node as usize;
    let mut mask = [0 as libc::c_ulong; MASK_WORDS];
    let Some(word) = mask.get_mut(at / bits) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: an error naming the node, once
            format!("NUMA node {node} is past the {} nodes a mask names", MASK_WORDS * bits),
        ));
    };
    *word |= 1 << (at % bits);
    // The kernel reads one bit fewer than `maxnode` says.
    let maxnode = (MASK_WORDS * bits + 1) as libc::c_ulong;
    // SAFETY: `mbind` reads `MASK_WORDS` words of `mask`, a live array, and changes only the
    // policy of the range, which the caller says is a reservation of ours.
    let status = timed(Kind::Advise, || unsafe {
        libc::syscall(
            libc::SYS_mbind,
            ptr.as_ptr(),
            bytes,
            MPOL_PREFERRED,
            mask.as_ptr(),
            maxnode,
            0,
        )
    });
    if status == 0 {
        return Ok(());
    }
    let why = io::Error::last_os_error();
    match why.raw_os_error() {
        Some(libc::ENOSYS) if node == 0 => Ok(()),
        // alloc: the error of a placement refused, once
        _ => Err(io::Error::new(
            why.kind(),
            format!("binding {bytes} bytes to NUMA node {node} (mbind): {why}"),
        )),
    }
}

/// Windows places a range when it is reserved, never after (`VirtualAllocExNuma`'s preferred
/// node is read only for a new reservation): an existing one is already where it is, which is
/// node `node` only on a machine of that one node.
#[cfg(windows)]
pub(crate) unsafe fn bind(_ptr: NonNull<u8>, _bytes: usize, node: u32) -> io::Result<()> {
    use windows_sys::Win32::System::Threading::GetNumaHighestNodeNumber;
    let mut highest = 0u32;
    // SAFETY: `highest` is a live `u32` to write.
    let known = unsafe { GetNumaHighestNodeNumber(&mut highest) } != 0;
    if known && highest == 0 && node == 0 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: an error naming the node, once
        format!(
            "Windows places memory on a NUMA node only when it is reserved: reserve this range on \
             node {node} (Region::reserve_on) rather than binding it after"
        ),
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) unsafe fn bind(_ptr: NonNull<u8>, _bytes: usize, node: u32) -> io::Result<()> {
    one_node(node)
}

/// Node 0, the one memory node there is where there is no placement call.
#[cfg(not(any(target_os = "linux", windows)))]
fn one_node(node: u32) -> io::Result<()> {
    if node == 0 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        // alloc: an error naming the node, once
        format!("{} has one memory node and no NUMA node {node}", std::env::consts::OS),
    ))
}

#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
fn whole(bytes: usize) -> io::Result<std::alloc::Layout> {
    std::alloc::Layout::from_size_align(bytes.max(1), RESERVE_ALIGN)
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "reservation overflows"))
}

/// Makes `[ptr, ptr + bytes)` readable and writable. On Linux it is already
/// ([`RESERVATION_WRITABLE`]), and this prefaults it (`MADV_POPULATE_WRITE`, Linux 5.14): the
/// pages about to be used made in one call that takes the `mmap_lock` for reading, rather than
/// one fault a page from whichever worker touches it first. Advice: a refusal (memory short
/// now) leaves the pages to their first touch, as before, so it is not an error.
///
/// # Safety
///
/// The range is page-aligned and inside one reservation of [`reserve`] that is not released.
/// Committing a range that is already committed is allowed and leaves its contents alone
/// (a populate never replaces a page that is there).
#[cfg(target_os = "linux")]
pub(crate) unsafe fn commit(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
    /// `mman-common.h`'s `MADV_POPULATE_WRITE`.
    const MADV_POPULATE_WRITE: libc::c_int = 23;
    // SAFETY: the caller says the range is inside a live reservation of ours, readable and
    // writable; populating makes zero pages where there are none and leaves the rest alone.
    let _ = timed(Kind::Populate, || unsafe {
        libc::madvise(ptr.as_ptr().cast(), bytes, MADV_POPULATE_WRITE)
    });
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) unsafe fn commit(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
    // SAFETY: the caller says the range is inside a live reservation of ours, which nothing
    // else maps; changing its protection touches no other memory.
    let rc = timed(Kind::Mprotect, || unsafe {
        libc::mprotect(ptr.as_ptr().cast(), bytes, libc::PROT_READ | libc::PROT_WRITE)
    });
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(windows)]
pub(crate) unsafe fn commit(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: the caller says the range is inside a live reservation of ours; committing
    // committed pages leaves them as they are.
    let got = unsafe { VirtualAlloc(ptr.as_ptr().cast(), bytes, MEM_COMMIT, PAGE_READWRITE) };
    if got.is_null() { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The whole reservation is memory already.
#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn commit(_ptr: NonNull<u8>, _bytes: usize) -> io::Result<()> {
    Ok(())
}

/// The sysfs file of the 2 MiB hugetlb pool's size.
#[cfg(target_os = "linux")]
const POOL: &str = "/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages";

/// Whether Linux makes transparent huge pages for a region advised `MADV_HUGEPAGE`: the
/// setting is `always` or `madvise`, not `never`, and the kernel has them at all.
#[cfg(target_os = "linux")]
pub(crate) fn transparent_offered() -> bool {
    // alloc: the setting's few bytes, read once per process
    match std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled") {
        Ok(setting) => !setting.contains("[never]"),
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn transparent_offered() -> bool {
    false
}

/// Checks the 2 MiB hugetlb pool has pages: a commit takes them from it, and a pool of none
/// would fail the first one.
#[cfg(target_os = "linux")]
pub(crate) fn reserved_pool() -> io::Result<()> {
    // alloc: the pool's size, read once per process
    let pages = std::fs::read_to_string(POOL).ok().and_then(|text| text.trim().parse::<u64>().ok());
    if pages.is_some_and(|pages| pages > 0) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: the error of a pool not set up, once
        format!(
            "{}=reserved, but the kernel reserves no 2 MiB pages ({POOL} is 0 or missing): \
             reserve them first (`sysctl vm.nr_hugepages=4096` holds 8 GiB), or unset it for \
             transparent huge pages",
            crate::huge::VAR,
        ),
    ))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn reserved_pool() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: the error of a setting this system cannot honor, once
        format!(
            "{}=reserved: reserved huge pages (hugetlbfs) are Linux's, and {} has none for \
             anonymous memory; unset it",
            crate::huge::VAR,
            std::env::consts::OS
        ),
    ))
}

/// Makes `[ptr, ptr + bytes)` readable and writable on 2 MiB pages of the reserved hugetlb
/// pool, every page made now, preferred on NUMA node `node` when given. Errs, with the range
/// given back to the reservation, when the pool has too few free pages: populating is what
/// takes them, so no later touch can fault for want of one (`MADV_POPULATE_WRITE`, Linux 5.14).
///
/// The range is also `MADV_DONTFORK`: a child made by `fork` gets no copy of it, so a write
/// after a fork never needs a page of the pool for copy-on-write. A child reaches none of the
/// region before it `exec`s.
///
/// # Safety
///
/// The range is 2 MiB-aligned at both ends, inside one live reservation of [`reserve`], not
/// committed, and reached by nothing until this returns; the caller makes no other commit of it
/// at once.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn commit_reserved(
    ptr: NonNull<u8>,
    bytes: usize,
    node: Option<u32>,
) -> io::Result<()> {
    /// `mman-common.h`'s `MADV_POPULATE_WRITE`.
    const MADV_POPULATE_WRITE: libc::c_int = 23;
    debug_assert!(ptr.as_ptr().addr() % crate::HUGE_PAGE == 0 && bytes % crate::HUGE_PAGE == 0);
    // SAFETY: the caller says the range is uncommitted space of a live reservation of ours
    // that nothing reaches; the fixed mapping replaces only it. `MAP_NORESERVE` defers the
    // pool's pages to the populate below, which reports their want as an error.
    let got = timed(Kind::MapFixed, || unsafe {
        libc::mmap(
            ptr.as_ptr().cast(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_FIXED
                | libc::MAP_PRIVATE
                | libc::MAP_ANONYMOUS
                | libc::MAP_NORESERVE
                | libc::MAP_HUGETLB
                | libc::MAP_HUGE_2MB,
            -1,
            0,
        )
    });
    if got == libc::MAP_FAILED {
        return Err(on_reserved(bytes, "mapping", &io::Error::last_os_error()));
    }
    let made = (|| {
        // SAFETY: the range just mapped, ours alone; the advice changes what `fork` copies.
        if timed(Kind::Advise, || unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_DONTFORK) }) != 0 {
            return Err(on_reserved(bytes, "excluding from fork", &io::Error::last_os_error()));
        }
        if let Some(node) = node {
            // SAFETY: the range just mapped, holding no page yet; the fresh mapping has the
            // default policy, which this replaces before the populate below makes its pages.
            unsafe { bind(ptr, bytes, node) }?;
        }
        // SAFETY: the range just mapped, readable and writable; populating writes zero pages
        // that nothing has read.
        if timed(Kind::Populate, || unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, MADV_POPULATE_WRITE) }) != 0 {
            return Err(on_reserved(bytes, "making the pages of", &io::Error::last_os_error()));
        }
        Ok(())
    })();
    if let Err(why) = made {
        // SAFETY: the same range, ours alone, whose contents nobody reads; this maps it back to
        // reserved space and returns what pages it took to the pool.
        unsafe { decommit(ptr, bytes, crate::huge::Pages::Reserved) }?;
        return Err(why);
    }
    Ok(())
}

/// Names a refused hugetlb step and what to do about it.
#[cfg(target_os = "linux")]
#[cold]
fn on_reserved(bytes: usize, step: &str, why: &io::Error) -> io::Error {
    let hint = match why.raw_os_error() {
        Some(libc::EINVAL) if step.starts_with("making") => {
            "populating needs Linux 5.14 (`MADV_POPULATE_WRITE`)"
        }
        Some(libc::ENOMEM | libc::EFAULT) => {
            "the pool has too few free 2 MiB pages: reserve more (`sysctl vm.nr_hugepages`)"
        }
        _ => "unset BLAZE_HUGE_PAGES for transparent huge pages",
    };
    // alloc: the error of a commit refused, once
    io::Error::new(
        why.kind(),
        format!("{step} {bytes} bytes on reserved 2 MiB pages (MAP_HUGETLB): {why}; {hint}"),
    )
}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn commit_reserved(
    _ptr: NonNull<u8>,
    _bytes: usize,
    _node: Option<u32>,
) -> io::Result<()> {
    reserved_pool()
}

/// Asks Linux to make `[ptr, ptr + bytes)`, committed and partly touched while it was made of
/// base pages, one huge page now, its contents kept (`MADV_COLLAPSE`, Linux 6.1). Advice: an
/// older kernel or a refusal leaves base pages, which khugepaged may collapse later.
///
/// # Safety
///
/// The range is 2 MiB-aligned, committed, and inside one live reservation of [`reserve`].
#[cfg(target_os = "linux")]
pub(crate) unsafe fn collapse(ptr: NonNull<u8>, bytes: usize) {
    /// `mman-common.h`'s `MADV_COLLAPSE`.
    const MADV_COLLAPSE: libc::c_int = 25;
    // SAFETY: the caller says the range is committed memory of ours; collapsing copies its
    // pages into one, keeping every byte, while other threads may read and write it.
    let _ = timed(Kind::Advise, || unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, MADV_COLLAPSE) });
}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn collapse(_ptr: NonNull<u8>, _bytes: usize) {}

/// A writable huge reservation ([`reserve`] on Linux) that grew past its first huge page:
/// its first 2 MiB, kept on base pages while it was all the region used, advised huge pages
/// like the rest and collapsed into one (two calls in a region's life, at that crossing).
///
/// # Safety
///
/// As for [`collapse`]: `ptr` is the reservation's base and `bytes` one huge page, committed.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn promote_first(ptr: NonNull<u8>, bytes: usize) {
    advise_pages(ptr.as_ptr(), bytes, true);
    // SAFETY: as the caller says, committed memory of ours.
    unsafe { collapse(ptr, bytes) };
}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn promote_first(_ptr: NonNull<u8>, _bytes: usize) {}

/// Gives `[ptr, ptr + bytes)` back to the kernel, still reserved, reading zeros when it is
/// committed again. On Linux the range stays readable and writable and its pages are dropped
/// (`MADV_DONTNEED`, the `mmap_lock` for reading; its advice and NUMA placement kept), but a
/// range of the reserved pool (`pages` [`Pages::Reserved`](crate::huge::Pages::Reserved)) is
/// mapped afresh over it, as base pages, so its 2 MiB pages go back to the pool and a later
/// commit can map the pool's again; then it says `true`: the new mapping lost the placement.
/// Elsewhere a fresh `PROT_NONE` mapping replaces it (`true`).
///
/// # Safety
///
/// As for [`commit`], and nothing reads or writes the range until it is committed again.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn decommit(
    ptr: NonNull<u8>,
    bytes: usize,
    pages: crate::huge::Pages,
) -> io::Result<bool> {
    if pages != crate::huge::Pages::Reserved {
        // SAFETY: the caller says the range is inside a live reservation of ours that nothing
        // uses; dropping its pages loses only contents nobody reads.
        let rc = timed(Kind::Purge, || unsafe {
            libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_DONTNEED)
        });
        return if rc == 0 { Ok(false) } else { Err(io::Error::last_os_error()) };
    }
    // SAFETY: as above; a fixed mapping over it replaces only those pages.
    let got = timed(Kind::MapFixed, || unsafe {
        libc::mmap(
            ptr.as_ptr().cast(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    });
    if got == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // The same range, mapped afresh just now: the new mapping lost the advice.
    advise_pages(ptr.as_ptr(), bytes, false);
    Ok(true)
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) unsafe fn decommit(
    ptr: NonNull<u8>,
    bytes: usize,
    _pages: crate::huge::Pages,
) -> io::Result<bool> {
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing uses;
    // a fixed mapping over it replaces only those pages.
    let got = timed(Kind::MapFixed, || unsafe {
        libc::mmap(
            ptr.as_ptr().cast(),
            bytes,
            libc::PROT_NONE,
            libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    });
    if got == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

#[cfg(windows)]
pub(crate) unsafe fn decommit(
    ptr: NonNull<u8>,
    bytes: usize,
    _pages: crate::huge::Pages,
) -> io::Result<bool> {
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing uses.
    let ok = unsafe { VirtualFree(ptr.as_ptr().cast(), bytes, MEM_DECOMMIT) };
    if ok != 0 { Ok(false) } else { Err(io::Error::last_os_error()) }
}

/// Gives the pages back through the runner, which leaves them reading zeros, as on Linux.
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
pub(crate) unsafe fn decommit(
    ptr: NonNull<u8>,
    bytes: usize,
    _pages: crate::huge::Pages,
) -> io::Result<bool> {
    // SAFETY: the caller says the range is inside a live reservation of ours, grown linear
    // memory, that nothing uses.
    unsafe { runner::decommit(ptr.as_ptr(), bytes) };
    Ok(false)
}

/// Keeps the memory and zeroes it, so a range committed again reads zeros as on Linux.
#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
pub(crate) unsafe fn decommit(
    ptr: NonNull<u8>,
    bytes: usize,
    _pages: crate::huge::Pages,
) -> io::Result<bool> {
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing
    // uses; the reservation is memory we own, so it is writable.
    unsafe { ptr.as_ptr().write_bytes(0, bytes) };
    Ok(false)
}

/// Lets the kernel take the pages of `[ptr, ptr + bytes)` back while they stay reserved: the
/// next touch after [`reuse`] finds them zeroed (Linux) or unspecified (macOS, Windows). Linux
/// drops them at once (`MADV_DONTNEED`), so the resident set falls now, not under pressure;
/// macOS marks them reusable (`MADV_FREE_REUSABLE`), which takes them out of the process's
/// footprint; Windows decommits them (`MEM_DECOMMIT`), which also gives back their charge
/// against the commit limit. Says whether the pages went: a refusal leaves them resident,
/// which costs memory and not correctness, and the caller must not count them as given back.
///
/// # Safety
///
/// As for [`commit`], the range is committed, and nothing holds data in it or touches it
/// before [`reuse`] is called over it.
#[cfg(unix)]
pub(crate) unsafe fn purge(ptr: NonNull<u8>, bytes: usize) -> bool {
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE_REUSABLE;
    #[cfg(not(target_os = "macos"))]
    let advice = libc::MADV_DONTNEED;
    // SAFETY: the caller says the committed range holds nothing anyone reads.
    timed(Kind::Purge, || unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, advice) }) == 0
}

#[cfg(windows)]
pub(crate) unsafe fn purge(ptr: NonNull<u8>, bytes: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    // A refusal leaves the pages committed, which costs memory and not correctness: `reuse`
    // commits them either way.
    // SAFETY: the caller says the committed range holds nothing anyone reads, and nothing
    // touches it before `reuse` commits it again.
    unsafe { VirtualFree(ptr.as_ptr().cast(), bytes, MEM_DECOMMIT) != 0 }
}

/// The pages go back through the runner, as for [`decommit`]; they read zeros after.
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
pub(crate) unsafe fn purge(ptr: NonNull<u8>, bytes: usize) -> bool {
    // SAFETY: the caller says the committed range holds nothing anyone reads.
    unsafe { runner::decommit(ptr.as_ptr(), bytes) };
    true
}

/// Nothing here gives pages back: the memory stays the process's, so none counts as given.
#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
pub(crate) unsafe fn purge(_ptr: NonNull<u8>, _bytes: usize) -> bool {
    false
}

/// Makes the pages of `[ptr, ptr + bytes)`, some of which [`purge`] may have given back, the
/// process's again before they are written: macOS counts pages marked reusable back into the
/// footprint (`MADV_FREE_REUSE`, which leaves other pages as they are), Windows commits the
/// decommitted ones (committing a committed page changes nothing). Linux and wasm fault them
/// back in on the first touch, so there it does nothing.
///
/// # Safety
///
/// As for [`commit`]: the range is inside a live, committed reservation of ours.
#[cfg(target_os = "macos")]
pub(crate) unsafe fn reuse(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
    // Advice: a refusal leaves the pages counted as reusable, which misstates the footprint
    // and costs no correctness, as libmalloc treats it.
    // SAFETY: the caller says the range is committed and ours.
    let _ = unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_FREE_REUSE) };
    Ok(())
}

#[cfg(windows)]
pub(crate) unsafe fn reuse(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
    // SAFETY: as for `commit`: the range is inside a live reservation of ours.
    unsafe { commit(ptr, bytes) }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) unsafe fn reuse(_ptr: NonNull<u8>, _bytes: usize) -> io::Result<()> {
    Ok(())
}

/// The most ranges one [`evict`] call hands the kernel: the pool's evictor gives back a batch
/// of up to 256 pages, merged into ranges, so a batch is one call (one TLB flush on Linux).
/// The `iovec`s live on the caller's stack, 4 KiB of them.
pub(crate) const EVICT_BATCH: usize = 256;

/// Gives the pages of each of `ranges` (byte offsets from `base`) back to the kernel, every byte
/// still readable and writable after, and says how many bytes went. The ranges go in order and a
/// refusal stops the call, so the bytes given back are those of a prefix of `ranges`, each range
/// whole; a refused range leaves its pages resident, which costs memory and not correctness.
///
/// - Linux: `MADV_DONTNEED`, so the pages leave the resident set and the cgroup's charge now and
///   read zeros when next touched. Each [`EVICT_BATCH`] ranges are one `process_madvise` of this
///   process (`PIDFD_SELF_THREAD_GROUP`, no descriptor to open or leak into a child), which
///   gathers the TLB once for the whole call (`vector_madvise` in `mm/madvise.c`) rather than
///   flushing every core running the process once per range, and takes the `mmap_lock` for
///   reading. It returns the bytes of the leading ranges it finished, which is this function's
///   prefix rule. [`check_evict`] proves once per process that the kernel takes it.
/// - macOS: `MADV_FREE_REUSABLE` per range, which takes the pages out of `phys_footprint` at
///   once; their contents are unspecified after, and [`reuse`] (`MADV_FREE_REUSE`) counts them
///   back before they are written.
/// - Windows: `MEM_DECOMMIT` per range, which also returns their commit charge. A decommitted
///   page faults when read, unlike the others: see [`crate::Region::evict`].
///
/// Never plain `MADV_FREE`: it is lazy, and the kernel goes on counting the pages (macOS's
/// footprint, Linux's resident set and cgroup charge) until it gets round to them, the "freed but
/// still counted" gap rule `one-memory-measure` forbids.
///
/// # Safety
///
/// Each range is page-aligned at both ends and inside one live sparse reservation of
/// [`reserve_sparse`] at `base`, and nothing reads or writes it through a plain reference
/// meanwhile (only [`crate::racy`]'s atomic copies, which may see any bytes).
#[cfg(target_os = "linux")]
pub(crate) unsafe fn evict(base: NonNull<u8>, ranges: &[std::ops::Range<usize>]) -> usize {
    let mut given = 0;
    for batch in ranges.chunks(EVICT_BATCH) {
        let mut iov = [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; EVICT_BATCH];
        let mut want = 0;
        for (slot, range) in iov.iter_mut().zip(batch) {
            slot.iov_base = base.as_ptr().wrapping_add(range.start).cast();
            slot.iov_len = range.len();
            want += range.len();
        }
        // SAFETY: `iov` holds `batch.len()` live entries the kernel only reads; each names a
        // range the caller says is page-aligned inside a live reservation of ours that nothing
        // references, so dropping its pages loses only bytes no one reads through a reference.
        let got = timed(Kind::Purge, || unsafe {
            libc::syscall(
                libc::SYS_process_madvise,
                PIDFD_SELF_THREAD_GROUP,
                iov.as_ptr(),
                batch.len(),
                libc::MADV_DONTNEED,
                0 as libc::c_uint,
            )
        });
        let Ok(got) = usize::try_from(got) else { break };
        given += got;
        if got != want {
            break;
        }
    }
    given
}

/// `fcntl.h`'s `PIDFD_SELF_THREAD_GROUP` (Linux 6.14): the calling process, as a pidfd, with
/// none opened. A forked child that called [`evict`] would name itself, never its parent, which
/// a pidfd opened by the parent and inherited would.
#[cfg(target_os = "linux")]
const PIDFD_SELF_THREAD_GROUP: libc::c_int = -10001;

#[cfg(target_os = "macos")]
pub(crate) unsafe fn evict(base: NonNull<u8>, ranges: &[std::ops::Range<usize>]) -> usize {
    let mut given = 0;
    for range in ranges {
        let at = base.as_ptr().wrapping_add(range.start);
        // SAFETY: the caller says the range is page-aligned inside a live reservation of ours
        // that nothing references; marking it reusable loses only bytes no one reads.
        let rc = timed(Kind::Purge, || unsafe {
            libc::madvise(at.cast(), range.len(), libc::MADV_FREE_REUSABLE)
        });
        if rc != 0 {
            break;
        }
        given += range.len();
    }
    given
}

/// The other Unixes have no reusable advice and no vector call: `MADV_DONTNEED` per range,
/// which drops a private anonymous page now there too.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) unsafe fn evict(base: NonNull<u8>, ranges: &[std::ops::Range<usize>]) -> usize {
    let mut given = 0;
    for range in ranges {
        let at = base.as_ptr().wrapping_add(range.start);
        // SAFETY: as on macOS.
        let rc = timed(Kind::Purge, || unsafe {
            libc::madvise(at.cast(), range.len(), libc::MADV_DONTNEED)
        });
        if rc != 0 {
            break;
        }
        given += range.len();
    }
    given
}

#[cfg(windows)]
pub(crate) unsafe fn evict(base: NonNull<u8>, ranges: &[std::ops::Range<usize>]) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    let mut given = 0;
    for range in ranges {
        let at = base.as_ptr().wrapping_add(range.start);
        // SAFETY: the caller says the range is inside a live reservation of ours that nothing
        // references or reads.
        if unsafe { VirtualFree(at.cast(), range.len(), MEM_DECOMMIT) } == 0 {
            break;
        }
        given += range.len();
    }
    given
}

/// Through the seed's runner, which leaves the pages reading zeros, as on Linux.
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
pub(crate) unsafe fn evict(base: NonNull<u8>, ranges: &[std::ops::Range<usize>]) -> usize {
    let mut given = 0;
    for range in ranges {
        // SAFETY: the caller says the range is inside a live reservation of ours, grown linear
        // memory, that nothing references.
        unsafe { runner::decommit(base.as_ptr().wrapping_add(range.start), range.len()) };
        given += range.len();
    }
    given
}

/// Nothing gives pages back here ([`check_evict`] says so).
#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
pub(crate) unsafe fn evict(_base: NonNull<u8>, _ranges: &[std::ops::Range<usize>]) -> usize {
    0
}

/// Whether [`check_evict`] passed in this process.
static EVICT_CHECKED: atomic::AtomicBool = atomic::AtomicBool::new(false);

/// Proves, once per process, that this kernel gives a sparse reservation's pages back through
/// [`evict`]: two pages of a fresh sparse reservation are written, given back in one call, and
/// must come back whole (and, on Linux, read zeros). On Linux that is `process_madvise` of the
/// calling process with `MADV_DONTNEED`, which needs 6.13 (any advice for the caller's own
/// process; before it only `MADV_COLD`, `MADV_PAGEOUT`, `MADV_WILLNEED` and `MADV_COLLAPSE`)
/// and 6.14 (`PIDFD_SELF_THREAD_GROUP`); blaze's floor is 7.2. An error names the call, what
/// the kernel said and its release: the store's pool stops on it, since without it no page it
/// loads would ever leave (rule `no-fallback`).
pub(crate) fn check_evict() -> io::Result<()> {
    if EVICT_CHECKED.load(atomic::Ordering::Acquire) {
        return Ok(());
    }
    probe_evict()?;
    EVICT_CHECKED.store(true, atomic::Ordering::Release);
    Ok(())
}

#[cfg(any(unix, windows, target_arch = "wasm64"))]
fn probe_evict() -> io::Result<()> {
    let page = page_size();
    let bytes = 2 * page;
    let base = reserve_sparse(bytes)?;
    for at in [0, page] {
        // SAFETY: `at` is inside the sparse reservation just made, readable and writable.
        unsafe { base.as_ptr().add(at).write_volatile(0xA5) };
    }
    let ranges = [0..page, page..bytes];
    // SAFETY: two page-aligned ranges of the fresh reservation, which nothing else knows of.
    let given = unsafe { evict(base, &ranges) };
    let why = (given != bytes).then(io::Error::last_os_error);
    // Only Linux promises what a page given back reads (zeros); a Windows page decommitted
    // faults when read.
    let kept = if cfg!(target_os = "linux") {
        // SAFETY: as above: on Linux the range stays readable and writable after a give-back.
        [0, page].map(|at| unsafe { base.as_ptr().add(at).read_volatile() })
    } else {
        [0, 0]
    };
    // SAFETY: the reservation just made, released whole, unused after.
    unsafe { release(base, bytes) };
    if let Some(why) = why {
        return Err(io::Error::new(
            why.kind(),
            // alloc: the error of a kernel that cannot give pages back, once
            format!(
                "{EVICT_CALL} gave back {given} of {bytes} bytes of this process's own sparse pages: {why}; \
                 {EVICT_NEEDS} (this is {})",
                system_release()
            ),
        ));
    }
    if kept != [0, 0] {
        return Err(io::Error::other(
            // alloc: as above
            format!(
                "{EVICT_CALL} said it gave back {bytes} bytes of this process's own pages, but they still \
                 hold what was written ({kept:?}): the pages did not leave ({})",
                system_release()
            ),
        ));
    }
    Ok(())
}

#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
fn probe_evict() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "without virtual memory a reservation is the global allocator's, and none of its pages can be given back",
    ))
}

#[cfg(target_os = "linux")]
const EVICT_CALL: &str = "process_madvise(PIDFD_SELF_THREAD_GROUP, MADV_DONTNEED)";
#[cfg(target_os = "linux")]
const EVICT_NEEDS: &str = "blaze needs Linux 6.13 for any advice to its own process and 6.14 for \
     PIDFD_SELF_THREAD_GROUP (its floor is 7.2)";
#[cfg(target_os = "macos")]
const EVICT_CALL: &str = "madvise(MADV_FREE_REUSABLE)";
#[cfg(windows)]
const EVICT_CALL: &str = "VirtualFree(MEM_DECOMMIT)";
#[cfg(all(
    any(unix, target_arch = "wasm64"),
    not(any(target_os = "linux", target_os = "macos"))
))]
const EVICT_CALL: &str = "giving pages back";
#[cfg(all(any(unix, windows, target_arch = "wasm64"), not(target_os = "linux")))]
const EVICT_NEEDS: &str = "the store's pool cannot give its pages back on this system";

/// The kernel's name and release, for an error: `uname`'s `sysname` and `release`.
#[cfg(unix)]
#[cold]
fn system_release() -> std::string::String {
    // SAFETY: `utsname` is plain bytes, valid zeroed.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `uname` writes the live struct it is given.
    if unsafe { libc::uname(&mut name) } != 0 {
        return "a kernel uname does not name".to_owned(); // alloc: an error's text, once
    }
    let text = |field: &[libc::c_char]| {
        let bytes = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8);
        let bytes: Vec<u8> = bytes.collect(); // alloc: an error's text, once
        std::string::String::from_utf8_lossy(&bytes).into_owned()
    };
    format!("{} {}", text(&name.sysname), text(&name.release)) // alloc: as above
}

#[cfg(any(windows, all(not(unix), target_arch = "wasm64")))]
#[cold]
fn system_release() -> std::string::String {
    std::env::consts::OS.to_owned() // alloc: an error's text, once
}

/// Releases a whole reservation.
///
/// # Safety
///
/// `ptr` and `bytes` are exactly what [`reserve`] returned and was asked for, and nothing uses
/// the range again.
#[cfg(unix)]
pub(crate) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
    let _ = timed(Kind::Munmap, || unsafe { libc::munmap(ptr.as_ptr().cast(), bytes) });
}

#[cfg(windows)]
pub(crate) unsafe fn release(ptr: NonNull<u8>, _bytes: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
    let _ = unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
}

/// Linear memory never shrinks: the pages go back through the runner, and the range, reading
/// zeros, to the free list for the next [`reserve`].
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
pub(crate) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    let pages = bytes.div_ceil(RESERVE_ALIGN).max(1);
    // SAFETY: the caller hands over the whole reservation, `pages` whole pages from `reserve`,
    // which nothing uses any more.
    unsafe { runner::decommit(ptr.as_ptr(), pages * RESERVE_ALIGN) };
    freed::give(ptr.as_ptr().addr() / RESERVE_ALIGN, pages);
}

#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
pub(crate) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    if let Ok(layout) = whole(bytes) {
        // SAFETY: the caller hands over exactly what `reserve` returned for `bytes`, which
        // `reserve` took from the global allocator with this same layout.
        unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) };
    }
}
