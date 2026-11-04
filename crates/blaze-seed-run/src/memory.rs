//! The seed's linear memory: address space reserved whole for the runner's maximum
//! ([`crate::Config::memory_max`]) when it is made, so it never moves, and pages made writable as it grows without being charged to the
//! commit limit, so a seed that grows to a TiB and touches a few GiB holds a few GiB.
//!
//! - **Reserve**: an anonymous `PROT_NONE` mapping, `MAP_NORESERVE`: address space and one
//!   mapping, no memory.
//! - **Grow**: `mprotect` of the new pages to read and write. On Linux the mapping is
//!   `VM_NORESERVE`, so making it writable charges no commit (under `vm.overcommit_memory` 0 or
//!   1; mode 2 ignores `MAP_NORESERVE` and charges every writable page, and growth past the
//!   limit fails naming it). macOS charges nothing for anonymous memory until it is touched.
//!   Either way a page costs memory only when the seed first writes it.
//! - **Decommit** ([`decommit`]): the seed gives pages back (blaze-mem's `os.rs` on wasm), and
//!   they read as zeros after.
//! - **Held** ([`held`]): how much of the memory the seed holds, from `mincore`: pages in RAM,
//!   and on macOS also pages the kernel paged out (into its compressor or to swap). A page the
//!   seed wrote leaves RAM only that way, so without them a machine short of memory would read
//!   as a seed holding less than it touched (55 MiB of 100 MiB under the runner's load).
//!   Linux's `mincore` does not say which pages went to swap; there a swapped page reads as
//!   not held.
//!
//! Windows has no runner: the seed bootstraps on Linux and macOS. `lib.rs` refuses it.

use std::{io, ptr::NonNull};

use wasmtime::{LinearMemory, MemoryCreator, MemoryType};

/// The host's page size.
fn page() -> usize {
    // SAFETY: `sysconf` reads a constant of the system.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).ok().filter(|page| page.is_power_of_two()).unwrap_or(4096)
}

/// `bytes` rounded up to `page`, a power of two.
fn round_up(bytes: usize, page: usize) -> Option<usize> {
    bytes.checked_add(page - 1).map(|bytes| bytes & !(page - 1))
}

/// The engine's memory creator: each memory a [`Reservation`].
pub(crate) struct Lazy;

// SAFETY: each memory is a fresh mapping of its own, zero-filled, owned by the `Reservation`
// Wasmtime holds until it drops it; nothing else maps or frees it.
unsafe impl MemoryCreator for Lazy {
    fn new_memory(
        &self,
        ty: MemoryType,
        minimum: usize,
        maximum: Option<usize>,
        reserved: Option<usize>,
        guard: usize,
    ) -> Result<Box<dyn LinearMemory>, String> {
        // dyn: Wasmtime's
        if guard != 0 {
            // alloc: error path
            return Err(format!(
                "Wasmtime asked the seed runner for a {guard}-byte guard region, which its engine sets to none"
            ));
        }
        let Some(reserved) = reserved else {
            // alloc: error path
            return Err("Wasmtime asked the seed runner for a memory with no reservation, which its engine always sets".to_owned());
        };
        if !ty.is_64() {
            // alloc: error path
            return Err(
                "the module's memory is 32-bit: the seed runner runs memory64 seeds (wasm64)"
                    .to_owned(),
            );
        }
        // The whole reservation, never less for a smaller maximum: Wasmtime compiles its bounds
        // checks against the engine's `memory_reservation`, which is this.
        let _ = maximum;
        if minimum > reserved {
            // alloc: error path
            return Err(format!(
                "the module's memory starts at {minimum} bytes, more than the {reserved} bytes the seed runner reserves (its maximum)"
            ));
        }
        // alloc: one per memory
        Ok(Box::new(
            Reservation::new(reserved, minimum).map_err(|why| {
                format!("reserving {reserved} bytes for the seed's memory: {why}")
            })?,
        ))
    }
}

/// One linear memory: `reserved` bytes of address space at `base`, the first `size` of them
/// readable and writable.
struct Reservation {
    base: NonNull<u8>,
    reserved: usize,
    size: usize,
    page: usize,
}

// SAFETY: the mapping is plain memory owned by this value alone; Wasmtime serializes access to
// it through its store, and moving it to another thread moves no thread-tied state.
unsafe impl Send for Reservation {}
// SAFETY: as above; `&Reservation` only reads `base`, `reserved` and `size`.
unsafe impl Sync for Reservation {}

impl Reservation {
    fn new(reserved: usize, minimum: usize) -> io::Result<Reservation> {
        let page = page();
        let reserved = round_up(reserved.max(page), page)
            .ok_or_else(|| io::Error::other("the reservation overflows"))?;
        // SAFETY: a fresh anonymous mapping at an address the kernel picks; it overlaps
        // nothing the process holds.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                reserved,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(base.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap returned null"))?;
        // Owned from here: an error below unmaps it.
        let mut memory = Reservation { base, reserved, size: 0, page };
        #[cfg(target_os = "linux")]
        no_huge_pages(base, reserved)?;
        memory.grow(minimum)?;
        Ok(memory)
    }

    /// Makes the pages up to `new` bytes writable.
    fn grow(&mut self, new: usize) -> io::Result<()> {
        if new > self.reserved {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                // alloc: error path
                format!("growing to {new} bytes, past the {} bytes reserved", self.reserved),
            ));
        }
        // Both are at most `reserved`, a multiple of the page, so neither rounds past it.
        let from = round_up(self.size, self.page).unwrap_or(self.reserved);
        let to = round_up(new, self.page).unwrap_or(self.reserved);
        if to > from {
            // SAFETY: `[base + from, base + to)` is inside the reservation (`to` is at most
            // `reserved`, a multiple of the page), which this value alone maps.
            let rc = unsafe {
                libc::mprotect(
                    self.base.as_ptr().add(from).cast(),
                    to - from,
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            };
            if rc != 0 {
                let why = io::Error::last_os_error();
                // alloc: error path
                return Err(io::Error::new(
                    why.kind(),
                    format!("making {} bytes writable (mprotect): {why}", to - from),
                ));
            }
        }
        self.size = new;
        Ok(())
    }
}

/// Keeps transparent huge pages off the reservation (`MADV_NOHUGEPAGE`). blaze-mem's wasm64
/// reservations are 64 KiB-aligned and lightly touched: under `transparent_hugepage=always`
/// each first touch of a 2 MiB stretch would fault in all of it, so the seed would hold far
/// more than it touches. A kernel without huge pages answers `EINVAL`: none to turn off.
#[cfg(target_os = "linux")]
fn no_huge_pages(base: NonNull<u8>, len: usize) -> io::Result<()> {
    // SAFETY: advice over the reservation just mapped, which only this value holds.
    if unsafe { libc::madvise(base.as_ptr().cast(), len, libc::MADV_NOHUGEPAGE) } == 0 {
        return Ok(());
    }
    let why = io::Error::last_os_error();
    if why.raw_os_error() == Some(libc::EINVAL)
        && !std::path::Path::new("/sys/kernel/mm/transparent_hugepage").exists()
    {
        return Ok(());
    }
    // alloc: error path
    Err(io::Error::new(
        why.kind(),
        format!(
            "turning transparent huge pages off for the seed's {len}-byte memory (madvise MADV_NOHUGEPAGE): {why}"
        ),
    ))
}

// SAFETY: `as_ptr` is the base of a mapping that never moves and stays mapped until drop;
// `byte_size` bytes from it are readable and writable, and `byte_capacity` bytes reserved.
unsafe impl LinearMemory for Reservation {
    fn byte_size(&self) -> usize {
        self.size
    }

    fn byte_capacity(&self) -> usize {
        self.reserved
    }

    fn grow_to(&mut self, new_size: usize) -> wasmtime::Result<()> {
        self.grow(new_size).map_err(wasmtime::Error::from)
    }

    fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // SAFETY: the whole mapping `new` made, which nothing uses once Wasmtime drops this.
        let _ = unsafe { libc::munmap(self.base.as_ptr().cast(), self.reserved) };
    }
}

/// Gives the pages of `[at, at + len)` back to the kernel; the range reads as zeros after. The
/// whole pages inside it are dropped (Linux: `MADV_DONTNEED`, which zero-fills private
/// anonymous memory; macOS: a fresh fixed mapping over them, as its `MADV_DONTNEED` keeps the
/// bytes); the parts of pages at its ends are written with zeros.
///
/// # Safety
///
/// The range is inside one readable and writable [`Reservation`], and nothing holds a
/// reference into it.
pub(crate) unsafe fn decommit(at: *mut u8, len: usize) -> io::Result<()> {
    let page = page();
    let start = at.addr();
    let end = start + len;
    let inner_start = round_up(start, page).unwrap_or(end).min(end);
    let inner_end = (end & !(page - 1)).max(inner_start);
    // SAFETY: both ends lie inside `[at, at + len)`, which the caller says is writable memory
    // no one references.
    unsafe {
        at.write_bytes(0, inner_start - start);
        at.add(inner_end - start).write_bytes(0, end - inner_end);
    }
    if inner_end == inner_start {
        return Ok(());
    }
    let inner = at.wrapping_add(inner_start - start);
    #[cfg(target_os = "linux")]
    // SAFETY: whole pages of a private anonymous mapping of ours that no one references; Linux
    // backs them with zero pages on the next touch.
    let rc = unsafe { libc::madvise(inner.cast(), inner_end - inner_start, libc::MADV_DONTNEED) };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: whole pages of a mapping of ours that no one references; the fixed mapping
    // replaces only them, with fresh zero pages, readable and writable as before.
    let rc = unsafe {
        let got = libc::mmap(
            inner.cast(),
            inner_end - inner_start,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
            -1,
            0,
        );
        if got == libc::MAP_FAILED { -1 } else { 0 }
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The `mincore` bits of a page the process holds. macOS says which pages it paged out (to its
/// compressor or swap), and a page the seed wrote is in one place or the other.
/// `MINCORE_PAGED_OUT` is `0x20` in xnu's `sys/mman.h`; the libc crate has no name for it.
#[cfg(target_os = "macos")]
const HELD: u8 = libc::MINCORE_INCORE as u8 | 0x20;
/// The `mincore` bit of a page in RAM: Linux says nothing of swap.
#[cfg(not(target_os = "macos"))]
const HELD: u8 = 1;

/// How many bytes of `[at, at + len)`, page-aligned at `at`, the process holds, from
/// `mincore`, a GiB at a time: in RAM, or on macOS paged out ([`HELD`]).
///
/// # Safety
///
/// The range is mapped.
pub(crate) unsafe fn held(at: *const u8, len: usize) -> io::Result<u64> {
    const WINDOW: usize = 1 << 30;
    let page = page();
    // alloc: one per measurement, a byte per page of a window
    let mut pages = vec![0u8; WINDOW / page];
    let mut held = 0u64;
    let mut done = 0;
    while done < len {
        let take = (len - done).min(WINDOW);
        let n = take.div_ceil(page);
        // SAFETY: `[at + done, at + done + take)` is mapped (the caller says the whole range
        // is), and `pages` has room for a byte per page of it.
        let window = unsafe { at.add(done) };
        // Linux takes the address as `*mut`, macOS as `*const`.
        #[cfg(target_os = "linux")]
        let window = window.cast_mut();
        // SAFETY: as above.
        let rc = unsafe { libc::mincore(window.cast(), take, pages.as_mut_ptr().cast()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        held += pages[..n].iter().filter(|&&p| p & HELD != 0).count() as u64 * page as u64;
        done += take;
    }
    Ok(held)
}
