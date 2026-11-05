//! The kernel's virtual memory: reserve address space, commit pages of it, give pages back,
//! release it. Linux and macOS through `mmap`/`mprotect`/`madvise`, Windows through
//! `VirtualAlloc`/`VirtualFree`. Every other function of the crate reaches the kernel through
//! these. On wasm there is no address space to reserve. On wasm64 (the bootstrap seed) a
//! reservation is linear memory grown at its end (`memory.grow`), which the seed's runner
//! (`blaze-seed-run`) commits only where it is touched, and pages go back through the runner's
//! `blaze_seed::decommit`. Elsewhere (wasm32, whose hosts have no such import) a reservation is
//! memory taken whole from the global allocator.
//!
//! A reservation is `PROT_NONE` (`PAGE_NOACCESS`, `MEM_RESERVE`): it costs address space and a
//! mapping, no memory. Committing makes a prefix readable and writable; Linux backs it with
//! pages on first touch, macOS and Windows also charge it to the commit limit. Every Unix
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
    reserve_with(bytes, huge, libc::PROT_NONE)
}

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
        return Err(io::Error::last_os_error());
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
    advise_pages(aligned, bytes, huge);
    #[cfg(not(target_os = "linux"))]
    let _ = huge;
    NonNull::new(aligned).ok_or_else(|| io::Error::other("mmap returned null"))
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
/// A fresh mapping over part of a range (a decommit's `MAP_FIXED`) loses the advice, so each
/// such mapping is advised again.
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

    use parking_lot::Mutex;

    /// First page to page count. A `BTreeMap` because merging a range with its neighbours
    /// needs them by address.
    static FREE: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

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

/// Makes `[ptr, ptr + bytes)` readable and writable.
///
/// # Safety
///
/// The range is page-aligned and inside one reservation of [`reserve`] that is not released.
/// Committing a range that is already committed is allowed and leaves its contents alone.
#[cfg(unix)]
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
        unsafe { decommit(ptr, bytes, false) }?;
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

/// Gives `[ptr, ptr + bytes)` back to the kernel and makes it inaccessible again, still
/// reserved.
///
/// # Safety
///
/// As for [`commit`], and nothing reads or writes the range until it is committed again.
#[cfg(unix)]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, huge: bool) -> io::Result<()> {
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
    // The same range, mapped afresh just now: the new mapping lost the advice.
    #[cfg(target_os = "linux")]
    advise_pages(ptr.as_ptr(), bytes, huge);
    #[cfg(not(target_os = "linux"))]
    let _ = huge;
    Ok(())
}

#[cfg(windows)]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, _huge: bool) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing uses.
    let ok = unsafe { VirtualFree(ptr.as_ptr().cast(), bytes, MEM_DECOMMIT) };
    if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Gives the pages back through the runner, which leaves them reading zeros, as on Linux.
#[cfg(all(not(any(unix, windows)), target_arch = "wasm64"))]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, _huge: bool) -> io::Result<()> {
    // SAFETY: the caller says the range is inside a live reservation of ours, grown linear
    // memory, that nothing uses.
    unsafe { runner::decommit(ptr.as_ptr(), bytes) };
    Ok(())
}

/// Keeps the memory and zeroes it, so a range committed again reads zeros as on Linux.
#[cfg(all(not(any(unix, windows)), not(target_arch = "wasm64")))]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, _huge: bool) -> io::Result<()> {
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing
    // uses; the reservation is memory we own, so it is writable.
    unsafe { ptr.as_ptr().write_bytes(0, bytes) };
    Ok(())
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
