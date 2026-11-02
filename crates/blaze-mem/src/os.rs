//! The kernel's virtual memory: reserve address space, commit pages of it, give pages back,
//! release it. Linux and macOS through `mmap`/`mprotect`/`madvise`, Windows through
//! `VirtualAlloc`/`VirtualFree`. Every other function of the crate reaches the kernel through
//! these. Elsewhere (wasm32) there is no address space to reserve, and a reservation is
//! memory taken whole.
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
/// virtual memory it is only the unit regions commit in.
#[cfg(not(unix))]
fn query_page_size() -> usize {
    4096
}

/// Reserves `bytes` (a multiple of [`page_size`]) of address space, aligned to
/// [`RESERVE_ALIGN`], none of it committed. With `huge`, the kernel is asked to back it with
/// transparent huge pages where it can.
#[cfg(unix)]
pub(crate) fn reserve(bytes: usize, huge: bool) -> io::Result<NonNull<u8>> {
    let padded = bytes
        .checked_add(RESERVE_ALIGN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "reservation overflows"))?;
    // SAFETY: a fresh anonymous mapping at an address the kernel picks: it overlaps nothing the
    // program holds.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            padded,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
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
        let _ = unsafe { libc::munmap(base.cast(), head) };
    }
    if tail != 0 {
        // SAFETY: `[aligned + bytes, base + padded)` is the back of the same fresh mapping.
        let _ = unsafe { libc::munmap(aligned.wrapping_add(bytes).cast(), tail) };
    }
    #[cfg(target_os = "linux")]
    if huge {
        // Advice only: with transparent huge pages off the kernel says EINVAL and the region
        // is made of base pages, which is what the machine offers.
        // SAFETY: `[aligned, aligned + bytes)` is the reservation just made.
        let _ = unsafe { libc::madvise(aligned.cast(), bytes, libc::MADV_HUGEPAGE) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = huge;
    NonNull::new(aligned).ok_or_else(|| io::Error::other("mmap returned null"))
}

#[cfg(windows)]
pub(crate) fn reserve(bytes: usize, _huge: bool) -> io::Result<NonNull<u8>> {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    // SAFETY: a fresh reservation at an address the kernel picks.
    let base = unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE, PAGE_NOACCESS) };
    NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)
}

/// Without virtual memory (wasm32's linear memory only grows at its end) a reservation is the
/// whole range taken at once, zeroed, from the global allocator: it never moves, as on the
/// kernels, but costs its full size from the start.
#[cfg(not(any(unix, windows)))]
pub(crate) fn reserve(bytes: usize, _huge: bool) -> io::Result<NonNull<u8>> {
    let layout = whole(bytes)?;
    // SAFETY: `layout` has a nonzero size (`whole` refuses zero).
    let base = unsafe { std::alloc::alloc_zeroed(layout) };
    NonNull::new(base).ok_or_else(|| {
        io::Error::new(io::ErrorKind::OutOfMemory, "the reservation is more than memory holds")
    })
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
        VirtualAllocExNuma(GetCurrentProcess(), std::ptr::null(), bytes, MEM_RESERVE, PAGE_NOACCESS, node)
    };
    NonNull::new(base.cast::<u8>()).ok_or_else(|| {
        let why = io::Error::last_os_error();
        // alloc: the error of a reservation refused, once
        io::Error::new(why.kind(), format!("reserving {bytes} bytes on NUMA node {node} (VirtualAllocExNuma): {why}"))
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
    let status = unsafe {
        libc::syscall(libc::SYS_mbind, ptr.as_ptr(), bytes, MPOL_PREFERRED, mask.as_ptr(), maxnode, 0)
    };
    if status == 0 {
        return Ok(());
    }
    let why = io::Error::last_os_error();
    match why.raw_os_error() {
        Some(libc::ENOSYS) if node == 0 => Ok(()),
        // alloc: the error of a placement refused, once
        _ => Err(io::Error::new(why.kind(), format!("binding {bytes} bytes to NUMA node {node} (mbind): {why}"))),
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

#[cfg(not(any(unix, windows)))]
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
    let rc = unsafe {
        libc::mprotect(ptr.as_ptr().cast(), bytes, libc::PROT_READ | libc::PROT_WRITE)
    };
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
pub(crate) unsafe fn commit_reserved(ptr: NonNull<u8>, bytes: usize, node: Option<u32>) -> io::Result<()> {
    /// `mman-common.h`'s `MADV_POPULATE_WRITE`.
    const MADV_POPULATE_WRITE: libc::c_int = 23;
    debug_assert!(ptr.as_ptr().addr() % crate::HUGE_PAGE == 0 && bytes % crate::HUGE_PAGE == 0);
    // SAFETY: the caller says the range is uncommitted space of a live reservation of ours
    // that nothing reaches; the fixed mapping replaces only it. `MAP_NORESERVE` defers the
    // pool's pages to the populate below, which reports their want as an error.
    let got = unsafe {
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
    };
    if got == libc::MAP_FAILED {
        return Err(on_reserved(bytes, "mapping", io::Error::last_os_error()));
    }
    let made = (|| {
        // SAFETY: the range just mapped, ours alone; the advice changes what `fork` copies.
        if unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_DONTFORK) } != 0 {
            return Err(on_reserved(bytes, "excluding from fork", io::Error::last_os_error()));
        }
        if let Some(node) = node {
            // SAFETY: the range just mapped, holding no page yet; the fresh mapping has the
            // default policy, which this replaces before the populate below makes its pages.
            unsafe { bind(ptr, bytes, node) }?;
        }
        // SAFETY: the range just mapped, readable and writable; populating writes zero pages
        // that nothing has read.
        if unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, MADV_POPULATE_WRITE) } != 0 {
            return Err(on_reserved(bytes, "making the pages of", io::Error::last_os_error()));
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
fn on_reserved(bytes: usize, step: &str, why: io::Error) -> io::Error {
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
    io::Error::new(why.kind(), format!("{step} {bytes} bytes on reserved 2 MiB pages (MAP_HUGETLB): {why}; {hint}"))
}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn commit_reserved(_ptr: NonNull<u8>, _bytes: usize, _node: Option<u32>) -> io::Result<()> {
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
    let _ = unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, MADV_COLLAPSE) };
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
    let got = unsafe {
        libc::mmap(
            ptr.as_ptr().cast(),
            bytes,
            libc::PROT_NONE,
            libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if got == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    if huge {
        // SAFETY: the same range, mapped afresh just now; the new mapping lost the advice.
        let _ = unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_HUGEPAGE) };
    }
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

/// Keeps the memory and zeroes it, so a range committed again reads zeros as on Linux.
#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, _huge: bool) -> io::Result<()> {
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing
    // uses; the reservation is memory we own, so it is writable.
    unsafe { ptr.as_ptr().write_bytes(0, bytes) };
    Ok(())
}

/// Lets the kernel take the pages of `[ptr, ptr + bytes)` back while they stay committed: the
/// next touch finds them zeroed (Linux) or unspecified (macOS, Windows).
///
/// # Safety
///
/// As for [`commit`], the range is committed, and nothing holds data in it.
#[cfg(unix)]
pub(crate) unsafe fn purge(ptr: NonNull<u8>, bytes: usize) {
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE_REUSABLE;
    #[cfg(not(target_os = "macos"))]
    let advice = libc::MADV_DONTNEED;
    // Advice: a refusal leaves the pages resident, which costs memory and not correctness.
    // SAFETY: the caller says the committed range holds nothing anyone reads.
    let _ = unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, advice) };
}

#[cfg(windows)]
pub(crate) unsafe fn purge(ptr: NonNull<u8>, bytes: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RESET, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: the caller says the committed range holds nothing anyone reads.
    let _ = unsafe { VirtualAlloc(ptr.as_ptr().cast(), bytes, MEM_RESET, PAGE_READWRITE) };
}

#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn purge(_ptr: NonNull<u8>, _bytes: usize) {}

/// Releases a whole reservation.
///
/// # Safety
///
/// `ptr` and `bytes` are exactly what [`reserve`] returned and was asked for, and nothing uses
/// the range again.
#[cfg(unix)]
pub(crate) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
    let _ = unsafe { libc::munmap(ptr.as_ptr().cast(), bytes) };
}

#[cfg(windows)]
pub(crate) unsafe fn release(ptr: NonNull<u8>, _bytes: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
    let _ = unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
}

#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    if let Ok(layout) = whole(bytes) {
        // SAFETY: the caller hands over exactly what `reserve` returned for `bytes`, which
        // `reserve` took from the global allocator with this same layout.
        unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) };
    }
}
