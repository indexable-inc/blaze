//! The kernel's virtual memory for a caller that lays its reservation out itself, each part
//! with its own protection: a confined guest's address space (`blaze-macro-eval`'s compiled
//! tier), reserved at an alignment of its choosing, committed, made read-only, decommitted
//! and released a range at a time. Committing, decommitting and releasing are the crate's own
//! (`os`); only reserving at a wider alignment than its reservations' and read-only ranges are
//! added. Elsewhere than Unix and Windows there is no address space to lay out, and
//! [`reserve_aligned`] says so.

use std::{io, ptr::NonNull};

/// Reserves `bytes` of address space at an address aligned to `align` (a power of two and a
/// multiple of the page size), inaccessible and none of it committed.
#[cfg(unix)]
pub fn reserve_aligned(bytes: usize, align: usize) -> io::Result<NonNull<u8>> {
    assert!(align.is_power_of_two(), "an alignment of {align} bytes");
    let padded = bytes
        .checked_add(align)
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "reservation overflows"))?;
    // SAFETY: a fresh anonymous mapping at an address the kernel picks: it overlaps nothing
    // the program holds.
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            padded,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if raw == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let addr = raw as usize;
    let aligned = addr.next_multiple_of(align);
    let head = aligned - addr;
    let tail = padded - head - bytes;
    if head != 0 {
        // SAFETY: `[raw, aligned)` is the front of the mapping just made, which nothing else
        // knows of.
        let _ = unsafe { libc::munmap(raw, head) };
    }
    if tail != 0 {
        // SAFETY: `[aligned + bytes, raw + padded)` is the back of the same mapping.
        let _ = unsafe { libc::munmap((aligned + bytes) as *mut libc::c_void, tail) };
    }
    NonNull::new(aligned as *mut u8).ok_or_else(|| io::Error::other("mmap gave address 0"))
}

/// How often another thread may take the aligned range between the release of the padded
/// reservation and the reservation of the range, before it is an error.
#[cfg(windows)]
const TRIES: u32 = 16;

/// Windows can't release part of a reservation, so a padded one finds the address, is
/// released, and the aligned range is reserved at it.
#[cfg(windows)]
pub fn reserve_aligned(bytes: usize, align: usize) -> io::Result<NonNull<u8>> {
    use windows_sys::Win32::System::Memory::{
        MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc, VirtualFree,
    };
    assert!(align.is_power_of_two(), "an alignment of {align} bytes");
    let padded = bytes
        .checked_add(align)
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "reservation overflows"))?;
    for _ in 0..TRIES {
        // SAFETY: a fresh reservation at an address the kernel picks.
        let raw = unsafe { VirtualAlloc(std::ptr::null(), padded, MEM_RESERVE, PAGE_NOACCESS) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let aligned = (raw as usize).next_multiple_of(align);
        // SAFETY: `raw` is the reservation just made, released whole and unused.
        unsafe { VirtualFree(raw, 0, MEM_RELEASE) };
        // SAFETY: a fresh reservation at an address no mapping of ours holds; the kernel
        // refuses it if another thread took the range meanwhile.
        let got = unsafe { VirtualAlloc(aligned as *const _, bytes, MEM_RESERVE, PAGE_NOACCESS) };
        if let Some(got) = NonNull::new(got.cast::<u8>()) {
            return Ok(got);
        }
    }
    // alloc: error path
    Err(io::Error::other(format!("another thread took the aligned address range {TRIES} times running")))
}

#[cfg(not(any(unix, windows)))]
pub fn reserve_aligned(bytes: usize, _align: usize) -> io::Result<NonNull<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("this target has no virtual memory to reserve {bytes} bytes of"), // alloc: error path
    ))
}

/// Makes `[ptr, ptr + len)` readable and writable, zeroed where it was never committed.
///
/// # Safety
///
/// The range is page-aligned and inside a live reservation of [`reserve_aligned`].
pub unsafe fn commit(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: the caller's contract is `os::commit`'s.
    unsafe { crate::os::commit(ptr, len) }
}

/// Makes `[ptr, ptr + len)` read-only.
///
/// # Safety
///
/// As for [`commit`], and the range is committed.
#[cfg(unix)]
pub unsafe fn protect_readonly(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: the caller says the range is inside our live reservation, which nothing else
    // maps; changing its protection touches no other memory.
    let rc = unsafe { libc::mprotect(ptr.as_ptr().cast(), len, libc::PROT_READ) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// # Safety
///
/// As for [`commit`], and the range is committed.
#[cfg(windows)]
pub unsafe fn protect_readonly(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let mut old = 0;
    // SAFETY: as for the Unix one; `old` is a live local to write.
    let ok = unsafe { VirtualProtect(ptr.as_ptr().cast(), len, PAGE_READONLY, &mut old) };
    if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// # Safety
///
/// None: there is no page to protect.
#[cfg(not(any(unix, windows)))]
pub unsafe fn protect_readonly(_ptr: NonNull<u8>, _len: usize) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "this target has no page protection"))
}

/// Gives `[ptr, ptr + len)` back and makes it inaccessible, still reserved; committed again,
/// it reads zeros.
///
/// # Safety
///
/// As for [`commit`], and nothing reads or writes the range until it is committed again.
pub unsafe fn decommit(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: the caller's contract is `os::decommit`'s, of a reservation without huge pages.
    unsafe { crate::os::decommit(ptr, len, false) }
}

/// Releases a whole reservation.
///
/// # Safety
///
/// `ptr` and `bytes` are what [`reserve_aligned`] gave and was asked for, and nothing uses the
/// range again.
#[cfg(any(unix, windows))]
pub unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
    unsafe { crate::os::release(ptr, bytes) }
}

/// [`reserve_aligned`] gives nothing here, so there is nothing to release.
///
/// # Safety
///
/// None: nothing is released.
#[cfg(not(any(unix, windows)))]
pub unsafe fn release(_ptr: NonNull<u8>, _bytes: usize) {}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    const ALIGN: usize = 1 << 30;
    const PAGE: usize = 64 << 10;

    #[test]
    fn a_reservation_is_aligned_and_commits_readonly_and_decommits_to_zeros() {
        let base = reserve_aligned(4 * PAGE, ALIGN).expect("address space");
        assert!(base.as_ptr().addr().is_multiple_of(ALIGN));
        // SAFETY: the first page of the live reservation just made, used only here.
        unsafe { commit(base, PAGE) }.expect("commit");
        // SAFETY: the page is committed and writable.
        unsafe { base.as_ptr().write(7) };
        // SAFETY: as above; nothing writes it after.
        unsafe { protect_readonly(base, PAGE) }.expect("read-only");
        // SAFETY: the page is committed and readable.
        assert_eq!(unsafe { base.as_ptr().read() }, 7);
        // SAFETY: nothing holds a reference into the page.
        unsafe { decommit(base, PAGE) }.expect("decommit");
        // SAFETY: the first page of the reservation, as at the start.
        unsafe { commit(base, PAGE) }.expect("commit again");
        // SAFETY: committed again, readable.
        assert_eq!(unsafe { base.as_ptr().read() }, 0);
        // SAFETY: the whole reservation, which nothing uses after.
        unsafe { release(base, 4 * PAGE) };
    }
}
